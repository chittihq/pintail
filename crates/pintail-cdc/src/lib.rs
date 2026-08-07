//! Native row-binlog CDC for Pintail.
//!
//! The stream buffers one source transaction, converts FULL before/after
//! images into versioned Pintail rows, synchronizes every touched table WAL,
//! and only then advances the `SQLite` source checkpoint. A crash therefore
//! replays at least once with deterministic versions.

mod ddl;
mod decoder;
mod gtid;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::DefaultHasher},
    fs::File,
    hash::{Hash as _, Hasher as _},
    io::{Seek as _, Write as _},
    path::{Path, PathBuf},
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
use pintail_probe::{ProbeReport, SourceFlavor, SourceTable, probe as probe_source};
use pintail_snapshot::{
    SnapshotError, SnapshotOptions, SnapshotPosition, SnapshotTarget, run_snapshot,
};
use pintail_store::{StoreError, StoreOptions, TableStore};
use pintail_types::{KeyMode, SchemaError, StoredRow};
use serde_json::json;
use thiserror::Error;

use crate::{
    ddl::{AlterKind, DdlAction, parse_ddl},
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
        // Compare at the store's catalog generation: live DDL (ALTER,
        // TRUNCATE) advances the durable schema version, and a version-1
        // rebuild would reject every store that ever evolved even though
        // the column layout still matches.
        let expected = source.table_schema_with_version(store.schema().version())?;
        if store.schema() != &expected {
            return Err(CdcError::InvalidConfiguration(format!(
                "store schema for {} does not match the probed source schema",
                source.name
            )));
        }
        Ok(Self { source, store })
    }

    /// Reopens a table using the latest durable stable-column IDs and schema
    /// generation recorded by the DDL tracker.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, schema history, or table storage cannot
    /// be opened consistently.
    pub fn open_tracked(
        metadata_path: &Path,
        database_id: &str,
        mut source: SourceTable,
        directory: impl AsRef<Path>,
        options: StoreOptions,
    ) -> Result<Self, CdcError> {
        let history = MetaStore::open(metadata_path)?.schema_history(database_id, &source.name)?;
        let version = history.last().map_or(1, |record| record.version);
        if let Some(record) = history.last() {
            source.columns = serde_json::from_str(&record.columns_json)
                .map_err(|error| CdcError::Ddl(error.to_string()))?;
        }
        let schema = source.table_schema_with_version(version)?;
        let store = TableStore::open(directory, schema.clone(), options)?;
        if store.schema() != &schema {
            return Err(CdcError::InvalidConfiguration(format!(
                "tracked store schema for {} differs from durable schema history",
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
    /// Maximum in-memory bytes retained before an uncommitted transaction
    /// spills to an anonymous temporary file.
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
    /// Auto-snapshot newly created source tables.
    pub auto_include_new_tables: bool,
    /// Optional case-insensitive allowlist for newly created tables. Empty
    /// means all tables not explicitly excluded.
    pub new_table_includes: BTreeSet<String>,
    /// Case-insensitive denylist for newly created tables.
    pub new_table_excludes: BTreeSet<String>,
    /// Parent directory for auto-included table stores. When absent, the
    /// first existing target's parent directory is used.
    pub new_table_root: Option<PathBuf>,
}

impl Default for CdcOptions {
    fn default() -> Self {
        Self {
            server_id: 0,
            blocking: true,
            max_commits: None,
            max_transaction_bytes: 256 * 1024 * 1024,
            max_reconnect_attempts: 8,
            reconnect_initial_delay: Duration::from_millis(100),
            auto_resnapshot: true,
            resnapshot_options: SnapshotOptions::default(),
            auto_include_new_tables: true,
            new_table_includes: BTreeSet::new(),
            new_table_excludes: BTreeSet::new(),
            new_table_root: None,
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
    /// A post-DDL source reprobe failed.
    #[error("CDC source reprobe failed: {0}")]
    Probe(#[from] pintail_probe::ProbeError),
    /// Probed schema or physical key failure.
    #[error("CDC schema failed: {0}")]
    Schema(#[from] SchemaError),
    /// A row event could not be decoded.
    #[error("CDC decode failed: {0}")]
    Decode(String),
    /// A source DDL statement could not be classified or applied safely.
    #[error("CDC schema tracking failed: {0}")]
    Ddl(String),
    /// An oversized transaction could not be written to or read from its
    /// anonymous spill file.
    #[error("CDC transaction spill failed: {0}")]
    TransactionSpill(String),
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
    let mut target_indexes = targets
        .iter()
        .enumerate()
        .map(|(index, target)| (target.source.name.to_ascii_lowercase(), index))
        .collect::<BTreeMap<_, _>>();
    let mut metadata = MetaStore::open(metadata_path)?;
    // Tables auto-included mid-stream are snapshotted at a position AHEAD
    // of the stream; row events at or before that position are already in
    // the snapshot and must not replay (append-row-id tables would
    // duplicate — keyed tables merely upsert, but the fence is exact for
    // both).
    let mut snapshot_fences: HashMap<usize, (String, u64)> = HashMap::new();
    for (name, &index) in &target_indexes {
        if let Some(stored) = metadata.setting(&fence_key(database_id, name))?
            && let Some((file, position_text)) = stored.rsplit_once(':')
            && let Ok(fence_position) = position_text.parse::<u64>()
        {
            snapshot_fences.insert(index, (file.to_owned(), fence_position));
        }
    }
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
    let resnapshot_context = AutoResnapshotContext {
        pool,
        metadata_path,
        database_id,
        report,
        enabled: options.auto_resnapshot,
        snapshot_options: &options.resnapshot_options,
    };
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
            Err(error @ CdcError::NeedsResync { .. }) => {
                position = resnapshot_context
                    .recover(
                        error,
                        &mut targets,
                        &mut blocked_targets,
                        &mut resnapshot_attempted,
                    )
                    .await?;
                pending = PendingTransaction::default();
                reconnect_attempts = 0;
                continue;
            }
            Err(CdcError::Mysql(error)) => {
                let reconnect = reconnect_from_checkpoint(
                    &metadata,
                    database_id,
                    report.server.flavor,
                    &options,
                    &mut reconnect_attempts,
                    error,
                )
                .await;
                position = match reconnect {
                    Ok(position) => position,
                    Err(error @ CdcError::NeedsResync { .. }) => {
                        reconnect_attempts = 0;
                        resnapshot_context
                            .recover(
                                error,
                                &mut targets,
                                &mut blocked_targets,
                                &mut resnapshot_attempted,
                            )
                            .await?
                    }
                    Err(error) => return Err(error),
                };
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
                    let fenced = snapshot_fences.get(&target_index).is_some_and(
                        |(fence_file, fence_pos)| {
                            position.file.as_str() < fence_file.as_str()
                                || (position.file == *fence_file && event_position <= *fence_pos)
                        },
                    );
                    if !fenced && snapshot_fences.remove(&target_index).is_some() {
                        // The stream passed the snapshot position; the fence
                        // has done its job across however many cycles it took.
                        metadata.delete_setting(&fence_key(
                            database_id,
                            &targets[target_index].source.name.to_ascii_lowercase(),
                        ))?;
                    }
                    if !fenced
                        && !blocked_targets.contains(&target_index)
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
                    let statement = query.query().into_owned();
                    let normalized = statement.trim().to_ascii_uppercase();
                    if normalized == "BEGIN" {
                        continue;
                    }
                    if normalized == "ROLLBACK" {
                        pending = PendingTransaction::default();
                    }
                    let actions = parse_ddl(&statement)?;
                    let tracks_schema = !actions.is_empty()
                        && (query.schema().is_empty()
                            || query.schema().eq_ignore_ascii_case(&report.database));
                    if tracks_schema && pending.has_mutations() {
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
                    if tracks_schema {
                        apply_ddl_actions(
                            pool,
                            metadata_path,
                            database_id,
                            report,
                            &mut targets,
                            &mut target_indexes,
                            &mut blocked_targets,
                            &mut snapshot_fences,
                            &mut metadata,
                            &options,
                            &statement,
                            actions,
                        )
                        .await?;
                    }
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
            let reconnect = reconnect_from_checkpoint(
                &metadata,
                database_id,
                report.server.flavor,
                &options,
                &mut reconnect_attempts,
                error,
            )
            .await;
            position = match reconnect {
                Ok(position) => position,
                Err(error @ CdcError::NeedsResync { .. }) => {
                    reconnect_attempts = 0;
                    resnapshot_context
                        .recover(
                            error,
                            &mut targets,
                            &mut blocked_targets,
                            &mut resnapshot_attempted,
                        )
                        .await?
                }
                Err(error) => return Err(error),
            };
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
        if pending.has_mutations() {
            return Err(CdcError::Decode(
                "binlog stream ended inside a source transaction".to_owned(),
            ));
        }
        return finish_result(commits, mutations, &position, targets);
    }
}

struct AutoResnapshotContext<'a> {
    pool: &'a Pool,
    metadata_path: &'a Path,
    database_id: &'a str,
    report: &'a ProbeReport,
    enabled: bool,
    snapshot_options: &'a SnapshotOptions,
}

impl AutoResnapshotContext<'_> {
    async fn recover(
        &self,
        error: CdcError,
        targets: &mut Vec<CdcTarget>,
        blocked_targets: &mut BTreeSet<usize>,
        attempted: &mut bool,
    ) -> Result<StreamPosition, CdcError> {
        if !self.enabled || *attempted {
            return Err(error);
        }
        let owned_targets = std::mem::take(targets);
        *targets = resnapshot_targets(
            self.pool,
            self.metadata_path,
            self.database_id,
            self.report,
            owned_targets,
            self.snapshot_options.clone(),
        )
        .await?;
        let checkpoint = MetaStore::open(self.metadata_path)?
            .snapshot_checkpoint(self.database_id)?
            .ok_or_else(|| {
                CdcError::InvalidCheckpoint(
                    "automatic resnapshot did not capture a source position".to_owned(),
                )
            })?;
        blocked_targets.clear();
        *attempted = true;
        StreamPosition::from_checkpoint(checkpoint, self.report.server.flavor)
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn apply_ddl_actions(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    targets: &mut Vec<CdcTarget>,
    target_indexes: &mut BTreeMap<String, usize>,
    blocked_targets: &mut BTreeSet<usize>,
    snapshot_fences: &mut HashMap<usize, (String, u64)>,
    metadata: &mut MetaStore,
    options: &CdcOptions,
    statement: &str,
    actions: Vec<DdlAction>,
) -> Result<(), CdcError> {
    let refreshed = probe_source(pool, &report.database).await?;
    for action in actions {
        match action {
            DdlAction::Alter {
                table,
                kind: AlterKind::AddOrDropColumns,
            } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        statement,
                        None,
                    )?;
                    continue;
                };
                let source = match stabilize_source_table(&targets[index].source, source) {
                    Ok(source) => source,
                    Err(reason) => {
                        quarantine_schema_change(
                            metadata,
                            database_id,
                            &targets[index],
                            index,
                            blocked_targets,
                            &format!("{statement}; {reason}"),
                            None,
                        )?;
                        continue;
                    }
                };
                let version = next_schema_version(targets[index].store.schema().version())?;
                let schema = source.table_schema_with_version(version)?;
                if let Err(error) = targets[index].store.evolve_schema(schema) {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        &format!("{statement}; {error}"),
                        Some(&source),
                    )?;
                    continue;
                }
                let columns_json = serde_json::to_string(&source.columns)
                    .map_err(|error| CdcError::Ddl(error.to_string()))?;
                metadata.record_schema_history(
                    database_id,
                    &table,
                    version,
                    Some(statement),
                    &columns_json,
                    &Utc::now().to_rfc3339(),
                )?;
                targets[index].source = source;
            }
            DdlAction::Alter {
                table,
                kind: AlterKind::RenameColumns(renames),
            } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        statement,
                        None,
                    )?;
                    continue;
                };
                // Apply the renames to the tracked source first so
                // name-matching carries each stable column ID to its new
                // spelling instead of treating the rename as drop-and-add.
                let mut previous = targets[index].source.clone();
                for (old_name, new_name) in &renames {
                    for column in &mut previous.columns {
                        if column.name.eq_ignore_ascii_case(old_name) {
                            column.name.clone_from(new_name);
                        }
                    }
                    for key in &mut previous.key.columns {
                        if key.eq_ignore_ascii_case(old_name) {
                            key.clone_from(new_name);
                        }
                    }
                }
                let source = match stabilize_source_table(&previous, source) {
                    Ok(source) => source,
                    Err(reason) => {
                        quarantine_schema_change(
                            metadata,
                            database_id,
                            &targets[index],
                            index,
                            blocked_targets,
                            &format!("{statement}; {reason}"),
                            None,
                        )?;
                        continue;
                    }
                };
                let version = next_schema_version(targets[index].store.schema().version())?;
                let schema = source.table_schema_with_version(version)?;
                if let Err(error) = targets[index].store.evolve_schema(schema) {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        &format!("{statement}; {error}"),
                        Some(&source),
                    )?;
                    continue;
                }
                let columns_json = serde_json::to_string(&source.columns)
                    .map_err(|error| CdcError::Ddl(error.to_string()))?;
                metadata.record_schema_history(
                    database_id,
                    &table,
                    version,
                    Some(statement),
                    &columns_json,
                    &Utc::now().to_rfc3339(),
                )?;
                targets[index].source = source;
            }
            DdlAction::Alter {
                table,
                kind: AlterKind::ModifyColumns(_),
            } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        statement,
                        None,
                    )?;
                    continue;
                };
                // Storage-compatible type changes evolve in place; anything
                // else fails stabilization (or the store's segment re-read)
                // and quarantines for resync exactly like before.
                let source = match stabilize_source_table(&targets[index].source, source) {
                    Ok(source) => source,
                    Err(reason) => {
                        quarantine_schema_change(
                            metadata,
                            database_id,
                            &targets[index],
                            index,
                            blocked_targets,
                            &format!("{statement}; {reason}"),
                            None,
                        )?;
                        continue;
                    }
                };
                let version = next_schema_version(targets[index].store.schema().version())?;
                let schema = source.table_schema_with_version(version)?;
                if let Err(error) = targets[index].store.evolve_schema(schema) {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        &format!("{statement}; {error}"),
                        Some(&source),
                    )?;
                    continue;
                }
                let columns_json = serde_json::to_string(&source.columns)
                    .map_err(|error| CdcError::Ddl(error.to_string()))?;
                metadata.record_schema_history(
                    database_id,
                    &table,
                    version,
                    Some(statement),
                    &columns_json,
                    &Utc::now().to_rfc3339(),
                )?;
                targets[index].source = source;
            }
            DdlAction::Alter {
                table,
                kind: AlterKind::IndexOnly,
            } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        statement,
                        None,
                    )?;
                    continue;
                };
                // Indexes have no storage representation here; adopt the
                // refreshed key metadata (unique keys, reconciliation flag)
                // without a schema generation. A changed key strategy fails
                // stabilization and quarantines like any other reshape.
                match stabilize_source_table(&targets[index].source, source) {
                    Ok(source) => {
                        targets[index].source = source;
                        let probe_json = serde_json::to_string(&refreshed)
                            .map_err(|error| CdcError::Ddl(error.to_string()))?;
                        metadata.refresh_database_probe_json(
                            database_id,
                            &probe_json,
                            &Utc::now().to_rfc3339(),
                        )?;
                    }
                    Err(reason) => {
                        quarantine_schema_change(
                            metadata,
                            database_id,
                            &targets[index],
                            index,
                            blocked_targets,
                            &format!("{statement}; {reason}"),
                            None,
                        )?;
                    }
                }
            }
            DdlAction::Alter {
                table,
                kind: AlterKind::RequiresResnapshot,
            } => {
                if let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) {
                    let source = find_source_table(&refreshed, &table);
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        statement,
                        source,
                    )?;
                }
            }
            DdlAction::Truncate { table } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let version = next_schema_version(targets[index].store.schema().version())?;
                let schema = targets[index].source.table_schema_with_version(version)?;
                targets[index].store.evolve_schema(schema)?;
                targets[index].store.reset_for_resnapshot()?;
                record_target_schema(metadata, database_id, &targets[index], version, statement)?;
            }
            DdlAction::Drop { table } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    continue;
                };
                let version = next_schema_version(targets[index].store.schema().version())?;
                record_target_schema(metadata, database_id, &targets[index], version, statement)?;
                metadata.mark_table_orphaned(
                    database_id,
                    &table,
                    statement,
                    &Utc::now().to_rfc3339(),
                )?;
                blocked_targets.insert(index);
            }
            DdlAction::Create { table } => {
                if !options.auto_include_new_tables
                    || !new_table_matches(&table, options)
                    || target_indexes.contains_key(&table.to_ascii_lowercase())
                {
                    continue;
                }
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    return Err(CdcError::Ddl(format!(
                        "created table {table} was absent from the refreshed source probe"
                    )));
                };
                let root = options
                    .new_table_root
                    .clone()
                    .or_else(|| {
                        targets
                            .first()
                            .and_then(|target| target.store.directory().parent())
                            .map(Path::to_path_buf)
                    })
                    .ok_or_else(|| {
                        CdcError::Ddl(
                            "auto-including a table requires a target storage root".to_owned(),
                        )
                    })?;
                let directory = new_table_directory(&root, &table);
                let store =
                    TableStore::open(directory, source.table_schema()?, StoreOptions::default())?;
                let snapshot_target = SnapshotTarget::new(source.clone(), store)?;
                let snapshot = run_snapshot(
                    pool,
                    metadata_path,
                    database_id,
                    &refreshed,
                    vec![snapshot_target],
                    options.resnapshot_options.clone(),
                )
                .await?;
                let target = snapshot.targets.into_iter().next().ok_or_else(|| {
                    CdcError::Ddl("new-table snapshot returned no target".to_owned())
                })?;
                let source = target.source().clone();
                let target = CdcTarget::new(source, target.into_store())?;
                let index = targets.len();
                targets.push(target);
                target_indexes.insert(table.to_ascii_lowercase(), index);
                // The fence must be the position captured under THIS
                // snapshot's read lock: the result's handoff position is
                // preserved from the original snapshot and sits far behind
                // the data actually copied. Durable because each supervisor
                // cadence is a fresh runner: an in-memory fence alone would
                // replay the next cycle.
                let fence = match &snapshot.captured_position {
                    SnapshotPosition::Gtid {
                        file: Some(file),
                        position: Some(fence_position),
                        ..
                    }
                    | SnapshotPosition::FilePosition {
                        file,
                        position: fence_position,
                    } => Some((file.clone(), *fence_position)),
                    SnapshotPosition::Gtid { .. } | SnapshotPosition::Unavailable => None,
                };
                if let Some((file, fence_position)) = fence {
                    metadata.set_setting(
                        &fence_key(database_id, &table.to_ascii_lowercase()),
                        &format!("{file}:{fence_position}"),
                    )?;
                    snapshot_fences.insert(index, (file, fence_position));
                }
                record_target_schema(metadata, database_id, &targets[index], 1, statement)?;
                // The stored probe report is the table inventory for both the
                // supervisor's next cycle and the query engine's catalog;
                // without this refresh the auto-included table vanishes from
                // both once this runner invocation ends.
                let probe_json = serde_json::to_string(&refreshed)
                    .map_err(|error| CdcError::Ddl(error.to_string()))?;
                metadata.refresh_database_probe_json(
                    database_id,
                    &probe_json,
                    &Utc::now().to_rfc3339(),
                )?;
            }
        }
    }
    Ok(())
}

/// Settings key persisting a mid-stream snapshot fence across runner cycles.
fn fence_key(database_id: &str, table: &str) -> String {
    format!("cdc_snapshot_fence:{database_id}:{table}")
}

fn find_source_table<'a>(report: &'a ProbeReport, table: &str) -> Option<&'a SourceTable> {
    report
        .tables
        .iter()
        .find(|source| source.name.eq_ignore_ascii_case(table))
}

/// Whether a column's declared type can change without touching stored
/// values: integer families share one 64-bit storage lane per signedness,
/// floats share the 64-bit carrier, string-typed columns are width-free
/// canonical text, and decimals render identically while the scale holds.
/// Temporal precisions are part of the type and must match exactly.
fn widening_compatible(
    previous: pintail_types::DataType,
    refreshed: pintail_types::DataType,
) -> bool {
    use pintail_types::DataType::{
        Boolean, Decimal, Float32, Float64, Int8, Int16, Int32, Int64, UInt8, UInt16, UInt32,
        UInt64,
    };
    if previous == refreshed {
        return true;
    }
    match (previous, refreshed) {
        (Boolean | Int8 | Int16 | Int32 | Int64, Boolean | Int8 | Int16 | Int32 | Int64)
        | (UInt8 | UInt16 | UInt32 | UInt64, UInt8 | UInt16 | UInt32 | UInt64)
        | (Float32 | Float64, Float32 | Float64) => true,
        (
            Decimal {
                precision: previous_precision,
                scale: previous_scale,
            },
            Decimal {
                precision: refreshed_precision,
                scale: refreshed_scale,
            },
        ) => previous_scale == refreshed_scale && refreshed_precision >= previous_precision,
        _ => false,
    }
}

fn stabilize_source_table(
    previous: &SourceTable,
    mut refreshed: SourceTable,
) -> Result<SourceTable, String> {
    if previous.key.mode != refreshed.key.mode
        || previous.key.columns.len() != refreshed.key.columns.len()
        || previous
            .key
            .columns
            .iter()
            .zip(&refreshed.key.columns)
            .any(|(left, right)| !left.eq_ignore_ascii_case(right))
    {
        return Err("physical key changed".to_owned());
    }
    let mut next_id = previous
        .columns
        .iter()
        .map(|column| column.id)
        .max()
        .unwrap_or(0);
    for column in &mut refreshed.columns {
        if let Some(existing) = previous
            .columns
            .iter()
            .find(|existing| existing.name.eq_ignore_ascii_case(&column.name))
        {
            if existing.pintail_type != column.pintail_type
                && !widening_compatible(existing.pintail_type, column.pintail_type)
            {
                // The probe reflects the source's CURRENT state, so an
                // earlier DDL event can legitimately see a later
                // storage-compatible widening; adopting the wider type
                // early is value-identical because row decode reads the
                // physical layout from the binlog table map.
                return Err(format!("column {} changed physical type", column.name));
            }
            column.id = existing.id;
        } else {
            next_id = next_id
                .checked_add(1)
                .ok_or_else(|| "stable column ID space is exhausted".to_owned())?;
            column.id = next_id;
        }
    }
    Ok(refreshed)
}

fn quarantine_schema_change(
    metadata: &mut MetaStore,
    database_id: &str,
    target: &CdcTarget,
    target_index: usize,
    blocked_targets: &mut BTreeSet<usize>,
    statement: &str,
    _refreshed: Option<&SourceTable>,
) -> Result<(), CdcError> {
    let version = next_schema_version(target.store.schema().version())?;
    let columns = target.source.columns.as_slice();
    let columns_json =
        serde_json::to_string(columns).map_err(|error| CdcError::Ddl(error.to_string()))?;
    metadata.record_schema_history(
        database_id,
        &target.source.name,
        version,
        Some(statement),
        &columns_json,
        &Utc::now().to_rfc3339(),
    )?;
    metadata.mark_table_needs_resync(database_id, &target.source.name, statement)?;
    blocked_targets.insert(target_index);
    Ok(())
}

fn record_target_schema(
    metadata: &mut MetaStore,
    database_id: &str,
    target: &CdcTarget,
    version: u32,
    statement: &str,
) -> Result<(), CdcError> {
    let columns_json = serde_json::to_string(&target.source.columns)
        .map_err(|error| CdcError::Ddl(error.to_string()))?;
    metadata.record_schema_history(
        database_id,
        &target.source.name,
        version,
        Some(statement),
        &columns_json,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

fn next_schema_version(version: u32) -> Result<u32, CdcError> {
    version
        .checked_add(1)
        .ok_or_else(|| CdcError::Ddl("table schema version exceeds UInt32".to_owned()))
}

fn new_table_directory(root: &Path, table: &str) -> PathBuf {
    let safe = table
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let mut hasher = DefaultHasher::new();
    table.to_ascii_lowercase().hash(&mut hasher);
    root.join(format!("table-{safe}-{:016x}", hasher.finish()))
}

fn new_table_matches(table: &str, options: &CdcOptions) -> bool {
    let included = options.new_table_includes.is_empty()
        || options
            .new_table_includes
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(table));
    let excluded = options
        .new_table_excludes
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(table));
    included && !excluded
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
    spill: Option<File>,
    spilled_mutations: usize,
    discarded_targets: BTreeSet<usize>,
    retained_bytes: usize,
    ordinal: u32,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PendingMutation {
    target_index: usize,
    row: StoredRow,
}

impl PendingTransaction {
    fn has_mutations(&self) -> bool {
        !self.mutations.is_empty() || self.spilled_mutations > 0
    }

    fn spill(&mut self, mutations: Vec<PendingMutation>) -> Result<(), CdcError> {
        if self.spill.is_none() {
            self.spill = Some(
                tempfile::tempfile()
                    .map_err(|error| CdcError::TransactionSpill(error.to_string()))?,
            );
            let retained = std::mem::take(&mut self.mutations);
            self.write_spilled(retained)?;
            self.retained_bytes = 0;
        }
        self.write_spilled(mutations)
    }

    fn write_spilled(&mut self, mutations: Vec<PendingMutation>) -> Result<(), CdcError> {
        let file = self.spill.as_mut().ok_or_else(|| {
            CdcError::TransactionSpill("spill file was not initialized".to_owned())
        })?;
        for mutation in mutations {
            serde_json::to_writer(&mut *file, &mutation)
                .map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
            file.write_all(b"\n")
                .map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
            self.spilled_mutations = self.spilled_mutations.saturating_add(1);
        }
        Ok(())
    }

    fn take_mutations(&mut self) -> Result<Vec<PendingMutation>, CdcError> {
        let mut mutations =
            Vec::with_capacity(self.spilled_mutations.saturating_add(self.mutations.len()));
        if let Some(file) = &mut self.spill {
            file.flush()
                .and_then(|()| file.rewind())
                .map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
            for mutation in
                serde_json::Deserializer::from_reader(file).into_iter::<PendingMutation>()
            {
                let mutation =
                    mutation.map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
                if !self.discarded_targets.contains(&mutation.target_index) {
                    mutations.push(mutation);
                }
            }
        }
        mutations.extend(
            self.mutations
                .drain(..)
                .filter(|mutation| !self.discarded_targets.contains(&mutation.target_index)),
        );
        Ok(mutations)
    }
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
    pending.discarded_targets.insert(target_index);
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
    let added_bytes = mutations.iter().fold(0_usize, |bytes, mutation| {
        bytes
            .saturating_add(mutation.row.estimated_bytes())
            .saturating_add(std::mem::size_of::<PendingMutation>())
    });
    if pending.spill.is_some() || pending.retained_bytes.saturating_add(added_bytes) > maximum_bytes
    {
        pending.spill(mutations)?;
    } else {
        pending.retained_bytes = pending.retained_bytes.saturating_add(added_bytes);
        pending.mutations.extend(mutations);
    }
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
    for mutation in pending.take_mutations()? {
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
    mut targets: Vec<CdcTarget>,
) -> Result<CdcResult, CdcError> {
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
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
    use super::{
        CdcOptions, PendingMutation, PendingTransaction, StreamPosition, generated_server_id,
        new_table_matches, push_mutations, sanitize_binlog_filename,
    };
    use pintail_meta::SnapshotCheckpointRecord;
    use pintail_probe::{SourceColumn, SourceFlavor, SourceKey, SourceTable};
    use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};

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

    #[test]
    fn new_table_allow_and_deny_rules_are_case_insensitive() {
        let mut options = CdcOptions::default();
        assert!(new_table_matches("events", &options));
        options.new_table_includes.insert("Events".to_owned());
        assert!(new_table_matches("events", &options));
        assert!(!new_table_matches("audit", &options));
        options.new_table_excludes.insert("EVENTS".to_owned());
        assert!(!new_table_matches("events", &options));
    }

    #[test]
    fn key_promotion_and_demotion_require_a_resnapshot_boundary() {
        let keyless = source_table(KeyMode::AppendRowId);
        let primary = source_table(KeyMode::Primary);
        assert_eq!(
            super::stabilize_source_table(&keyless, primary.clone()),
            Err("physical key changed".to_owned())
        );
        assert_eq!(
            super::stabilize_source_table(&primary, keyless),
            Err("physical key changed".to_owned())
        );
    }

    fn source_table(mode: KeyMode) -> SourceTable {
        SourceTable {
            name: "events".to_owned(),
            engine: Some("InnoDB".to_owned()),
            estimated_rows: Some(2),
            columns: vec![SourceColumn {
                id: 1,
                name: "id".to_owned(),
                mysql_data_type: "bigint".to_owned(),
                mysql_column_type: "bigint".to_owned(),
                pintail_type: DataType::Int64,
                nullable: false,
                character_set: None,
                collation: None,
                generated_stored: false,
                auto_increment: false,
                default_value: None,
                default_generated: false,
            }],
            key: SourceKey {
                mode,
                index_name: (mode != KeyMode::AppendRowId).then(|| "PRIMARY".to_owned()),
                columns: if mode == KeyMode::AppendRowId {
                    Vec::new()
                } else {
                    vec!["id".to_owned()]
                },
            },
            unique_keys: Vec::new(),
            requires_reconciliation: false,
            foreign_keys: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn oversized_transactions_spill_and_round_trip() {
        let mut pending = PendingTransaction::default();
        let row = StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(7)]).expect("key"),
            vec![Value::Utf8("large payload".repeat(32))],
            9,
            false,
        );
        push_mutations(
            &mut pending,
            vec![PendingMutation {
                target_index: 2,
                row: row.clone(),
            }],
            1,
        )
        .expect("spill mutation");

        assert!(pending.spill.is_some());
        assert!(pending.mutations.is_empty());
        let mutations = pending.take_mutations().expect("read spill");
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].target_index, 2);
        assert_eq!(mutations[0].row, row);
    }
}
