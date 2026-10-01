//! Native row-binlog CDC for Pintail.
//!
//! The stream buffers one source transaction, converts FULL before/after
//! images into versioned Pintail rows, and closes it into the open batch.
//! A batch of whole transactions is written to every touched table WAL,
//! synchronized, and only then does one `SQLite` checkpoint advance the
//! source position past all of them. A crash therefore replays at least once
//! with deterministic versions.

mod ddl;
mod decoder;
mod event;
pub use event::{TRANSACTION_PAYLOAD_EVENT, check_transaction_payload_header};

const ROTATE_EVENT: u8 = 0x04;
const FORMAT_DESCRIPTION_EVENT: u8 = 0x0f;
mod gtid;
#[cfg(test)]
mod simulation;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque, hash_map::DefaultHasher},
    fs::File,
    hash::{Hash as _, Hasher as _},
    io::{BufReader, BufWriter, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::Utc;
use futures_util::{FutureExt as _, StreamExt as _};
use mysql_async::{
    BinlogStream, BinlogStreamRequest, Error as MysqlError, Pool,
    binlog::{
        EventFlags, RowsEventFlags,
        events::{EventData, RowsEventData},
        row::BinlogRow,
    },
    prelude::Queryable as _,
};
use pintail_meta::{CdcApplyIntent, MetaStore, SnapshotCheckpointRecord};
use pintail_probe::{ProbeReport, SourceFlavor, SourceTable, probe as probe_source};
use pintail_snapshot::{
    SnapshotError, SnapshotOptions, SnapshotPosition, SnapshotTarget, run_snapshot,
};
use pintail_store::{StoreError, StoreOptions, TableStore};
use pintail_types::{KeyMode, SchemaError, StoredRow, Value};
use serde_json::json;
use thiserror::Error;

use crate::{
    ddl::{AlterKind, DdlAction, parse_ddl},
    decoder::{RowAlignment, UnknownColumn, decode_row, image_ordinals, insert_key, physical_key},
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
        let mut metadata = MetaStore::open(metadata_path)?;
        let history = metadata.schema_history(database_id, &source.name)?;
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
        if history.is_empty() {
            freeze_first_generation(&mut metadata, database_id, &source)?;
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
    /// How long a finite catch-up may run before it returns at the next
    /// commit. A source written faster than the stream applies never reaches
    /// the end of its binlog, so without this one catch-up runs for as long
    /// as the backlog lasts and whatever waits on its return waits with it.
    pub max_duration: Option<Duration>,
    /// Ends the catch-up at the next commit when someone outside asks.
    pub stop: Option<CycleStop>,
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
            max_duration: None,
            stop: None,
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

/// A request from outside the stream to end a catch-up early.
///
/// The stream looks at it after each commit, so the position it returns is
/// always the end of a whole source transaction.
#[derive(Clone, Debug, Default)]
pub struct CycleStop(Arc<AtomicBool>);

impl CycleStop {
    /// A signal nobody has raised yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the stream holding this signal to return at its next commit.
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether a stop has been asked for.
    #[must_use]
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl PartialEq for CycleStop {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CycleStop {}

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
    /// A failure confined to one table's own storage or copy: the stream
    /// itself is sound, and the supervisor quarantines that table instead
    /// of failing every table the database mirrors.
    #[error("{source} (table {table})")]
    Table {
        /// The source table whose storage or copy failed.
        table: String,
        /// What failed.
        source: Box<CdcError>,
    },
}

impl CdcError {
    /// Scopes this error to `table`.
    #[must_use]
    pub fn for_table(self, table: &str) -> Self {
        match self {
            scoped @ Self::Table { .. } => scoped,
            other => Self::Table {
                table: table.to_owned(),
                source: Box::new(other),
            },
        }
    }

    /// The one table this failure is confined to, when it is.
    #[must_use]
    pub fn failing_table(&self) -> Option<&str> {
        match self {
            Self::Table { table, .. } => Some(table),
            _ => None,
        }
    }
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
    // An operator action waiting for this stream must not also wait for the
    // merges its tables have running when they close.
    if let Some(stop) = &options.stop {
        for target in &mut targets {
            target.store.yield_merges_to(Arc::clone(&stop.0));
        }
    }
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
    // Tables whose drift heal already failed once. A source that keeps
    // disagreeing must not be re-probed per row: one attempt, then the
    // decoder's quarantine path owns it.
    let mut drift_heals: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (name, &index) in &target_indexes {
        if let Some(stored) = metadata.setting(&fence_key(database_id, name))?
            && let Some((file, position_text)) = stored.rsplit_once(':')
            && let Ok(fence_position) = position_text.parse::<u64>()
        {
            snapshot_fences.insert(index, (file.to_owned(), fence_position));
        }
    }
    // A paused table is blocked the same way a quarantined one is: its row
    // events are passed over, the position still advances, and nothing is
    // queued for it. The events it misses are gone for good, which is why
    // resuming it goes through a resync rather than a replay.
    let paused_targets = metadata
        .paused_tables(database_id)?
        .iter()
        .filter_map(|name| target_indexes.get(&name.to_ascii_lowercase()).copied())
        .collect::<BTreeSet<_>>();
    // Paused targets whose skipped changes this run has already recorded.
    let mut paused_skipped = BTreeSet::new();
    let mut blocked_targets = metadata
        .tables_needing_resync(database_id)?
        .iter()
        .filter_map(|name| target_indexes.get(&name.to_ascii_lowercase()).copied())
        .chain(paused_targets.iter().copied())
        .collect::<BTreeSet<_>>();
    let checkpoint = metadata
        .snapshot_checkpoint(database_id)?
        .ok_or_else(|| CdcError::InvalidCheckpoint("snapshot position is absent".to_owned()))?;
    let mut position = StreamPosition::from_checkpoint(checkpoint, report.server.flavor)?;
    // The resumed position is the single most useful line in a replication
    // log: a mirror that looks stalled is usually one that resumed from an
    // older checkpoint than the operator assumed. It earns that only when
    // something about it CHANGED, though - a supervised pass is not
    // blocking, so one runs every few seconds and an unconditional line here
    // prints hundreds of times an hour per database and buries the log it
    // was meant to clarify. The position itself advances on every pass by
    // definition and is left out of the comparison for that reason.
    let summary = format!(
        "targets={} blocked={} paused={} file={} gtid={}",
        targets.len(),
        blocked_targets.len(),
        paused_targets.len(),
        position.file,
        // Presence only. A GTID set names every transaction the replica has
        // seen and grows without bound on a busy source, so printing it would
        // swamp the log it is meant to clarify.
        position.gtid_set.as_ref().map_or("none", |_| "present")
    );
    if start_summary_changed(database_id, &summary) {
        pintail_log::log_info!("cdc start db={database_id} {summary} pos={}", position.pos);
    } else {
        pintail_log::log_debug!("cdc start db={database_id} {summary} pos={}", position.pos);
    }
    let server_id = if options.server_id == 0 {
        generated_server_id(database_id)
    } else {
        options.server_id
    };
    let mut pending = PendingTransaction::default();
    let mut batch = ApplyBatch::default();
    let mut durable;
    let mut phases = PhaseTimes::default();
    let mut commits = 0_usize;
    let cycle_started = Instant::now();
    let mut mutations = 0_usize;
    let mut events_read = 0_usize;
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
        // Every path back here - start, reconnect, recopy - rebuilt the
        // position from its checkpoint, which does not carry the floor.
        position.floor = position.floor.max(resume_floor(
            &metadata,
            database_id,
            &position,
            stored_version_floor(&targets),
        )?);
        durable = DurablePoint {
            file: position.file.clone(),
            pos: position.pos,
            floor: position.floor,
        };
        // Writes out the closed transactions waiting in the batch: one WAL
        // record and one sync per touched table, one checkpoint for all of
        // them. Every place that needs the stores and the checkpoint to
        // agree with the stream - a schema change, a return, a reconnect -
        // calls it first.
        macro_rules! flush {
            () => {
                if !batch.is_empty() {
                    let flushed = flush_batch(
                        &mut targets,
                        &mut metadata,
                        database_id,
                        &position,
                        &mut batch,
                        &mut durable,
                        &mut phases,
                    )?;
                    commits += flushed.transactions;
                    mutations += flushed.mutations;
                    for index in flushed.passed_fences {
                        // The durable position is past the snapshot's; the
                        // fence has done its job across however many cycles
                        // it took.
                        if snapshot_fences.remove(&index).is_some() {
                            metadata.delete_setting(&fence_key(
                                database_id,
                                &targets[index].source.name.to_ascii_lowercase(),
                            ))?;
                        }
                    }
                    progress(CdcProgress {
                        commits,
                        mutations,
                        checkpoint: flushed.checkpoint,
                    });
                }
            };
        }
        // Closes the open transaction into the batch. One that spilled is
        // staged in the stores a piece at a time, and the transactions
        // before it have to be stored first: see `seal_transaction`.
        macro_rules! seal {
            () => {
                if pending.spill.is_some() {
                    flush!();
                }
                seal_transaction(
                    &mut position,
                    &mut pending,
                    &mut batch,
                    &mut targets,
                    STAGE_BYTES,
                )?;
            };
        }
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
                        &paused_targets,
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
                                &paused_targets,
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
        let mut out_of_order = None;
        // MariaDB writes end_log_pos 0 on every event inside a transaction -
        // table maps and row events included - so those events take the last
        // real position read, which is the transaction's own GTID event. The
        // raw zero made every transaction's row versions identical: rows of a
        // keyless table, keyed by version, overwrote one another, and a row
        // event compared against a snapshot fence always fell below it.
        let mut logged_position = position.pos;
        loop {
            // A stream with nothing more to hand over right now is the end
            // of the batch: the transactions already closed are written out
            // before the wait, so a quiet source is mirrored as each
            // transaction arrives and only a busy one is batched.
            let mut waiting = Instant::now();
            let next = if batch.is_empty() {
                stream.next().await
            } else if let Some(next) = stream.next().now_or_never() {
                next
            } else {
                flush!();
                waiting = Instant::now();
                stream.next().await
            };
            phases.read += waiting.elapsed();
            let Some(event) = next else {
                break;
            };
            let closed_before = commits + batch.transactions;
            let event = match event {
                Ok(event) => {
                    reconnect_attempts = 0;
                    events_read += 1;
                    event
                }
                Err(error) => {
                    stream_error = Some(self::event::stream_error(error)?);
                    break;
                }
            };
            let event_type = event.header().event_type_raw();
            let logged = u64::from(event.header().log_pos());
            // The connection preamble's rotate and format description describe
            // the file, not the stream's progress.
            if logged > 0 && !matches!(event_type, ROTATE_EVENT | FORMAT_DESCRIPTION_EVENT) {
                logged_position = logged;
            }
            let event_position = if logged > 0 { logged } else { logged_position };
            let Some(data) = self::event::decode_event(&event)? else {
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
                    if let Some(reason) = position.out_of_order(event_position)? {
                        flush!();
                        metadata.mark_database_needs_resync(database_id, &reason)?;
                        out_of_order = Some(CdcError::NeedsResync { reason });
                        break;
                    }
                }
                EventData::RotateEvent(rotate) => {
                    // The artificial rotate that opens a stream is not a
                    // rotation. MySQL sends it with position zero; MariaDB
                    // sends it with the requested position, and before the
                    // format description arrives its checksum bytes are still
                    // on the name - printable ones became part of the file
                    // name. The header flag names it on both servers.
                    let artificial = event
                        .header()
                        .flags()
                        .contains(EventFlags::LOG_EVENT_ARTIFICIAL_F);
                    if !rotate.is_fake() && !artificial {
                        position.file = sanitize_binlog_filename(&rotate.name())?;
                        position.pos = rotate.position();
                    } else if matches!(position.kind, PositionKind::MysqlGtid)
                        && !rotate.name().is_empty()
                    {
                        // A GTID resume names no file; the source picks one,
                        // and this preamble is the only place it says which.
                        // MySQL's carries a clean name.
                        position.file = sanitize_binlog_filename(&rotate.name())?;
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
                    if !fenced && snapshot_fences.contains_key(&target_index) {
                        // The stream passed the snapshot position. The fence
                        // goes once the checkpoint has passed it too: lifted
                        // here, a replay from a checkpoint still behind the
                        // snapshot would apply rows the snapshot holds.
                        pending.passed_fences.insert(target_index);
                    }
                    // Placing the row image against the tracked schema is what
                    // detects a missed schema change: it succeeds for every
                    // row a caught-up stream sees, and the ways it fails are
                    // the ways DDL can reach the table without reaching this
                    // stream. Re-probe on that failure rather than on a width
                    // comparison - once several ALTERs have landed the widths
                    // disagree even on rows that place perfectly well.
                    let live = !fenced && !blocked_targets.contains(&target_index);
                    if !fenced
                        && paused_targets.contains(&target_index)
                        && paused_skipped.insert(target_index)
                    {
                        // The first change passed over for a paused table
                        // is what turns its resume into a recopy.
                        metadata.mark_table_paused_skipped(
                            database_id,
                            &targets[target_index].source.name,
                        )?;
                    }
                    let mut alignment = live.then(|| {
                        RowAlignment::resolve(
                            &targets[target_index].source,
                            table_map,
                            UnknownColumn::Reject,
                        )
                    });
                    if matches!(alignment, Some(Err(_))) {
                        let row_columns =
                            usize::try_from(table_map.columns_count()).unwrap_or(usize::MAX);
                        // One probe per table per row width. A source that is
                        // genuinely broken keeps failing on the same width, and
                        // must not probe-storm MySQL once per row it sends.
                        if drift_heals.insert((target_index, row_columns)) {
                            // The heal changes the store's schema, and the
                            // rows waiting in the batch were shaped for the
                            // one before it.
                            flush!();
                            heal_schema_drift(
                                pool,
                                &report.database,
                                database_id,
                                &mut targets[target_index],
                                &mut metadata,
                                table_map,
                            )
                            .await;
                        }
                        // Re-ask whether or not the probe ran. The two are
                        // independent: the probe refreshes the schema, while
                        // the second ask accepts a column the image carries
                        // and the schema has genuinely dropped. An image older
                        // than an adopted DROP needs only the latter, and
                        // tying it to the probe stranded exactly that case
                        // once the width had been tried.
                        alignment = Some(RowAlignment::resolve(
                            &targets[target_index].source,
                            table_map,
                            UnknownColumn::Ignore,
                        ));
                    }
                    match alignment {
                        Some(Ok(alignment)) => {
                            let decoding = Instant::now();
                            let failed = decode_rows_event(
                                &rows_event,
                                table_map,
                                &targets[target_index].source,
                                &alignment,
                                target_index,
                                &position,
                                event_position,
                                event_type,
                                database_id,
                                &metadata,
                                &mut pending,
                                options.max_transaction_bytes,
                            )?;
                            phases.decode += decoding.elapsed();
                            if failed {
                                blocked_targets.insert(target_index);
                            }
                        }
                        Some(Err(error)) => {
                            // The row cannot be placed even against a freshly
                            // probed schema. Resync is the honest outcome, and
                            // the reason travels with it: under MINIMAL
                            // metadata this is where a stream that fell more
                            // than one schema change behind ends up, and the
                            // operator needs to see that rather than a bare
                            // "needs resync".
                            let reason = error.to_string();
                            // The end of the line for this table, and the only
                            // place that says so. A heal that was skipped
                            // because its width had already been tried logs
                            // nothing of its own, so without this the table
                            // just stops replicating with no stated cause.
                            pintail_log::log_error!(
                                "cdc drift unrecoverable db={database_id} table={} at {}:{}: \
                                 {reason}",
                                targets[target_index].source.name,
                                position.file,
                                event_position
                            );
                            record_dlq(
                                &metadata,
                                database_id,
                                &targets[target_index].source.name,
                                &position,
                                EventLocation {
                                    position: event_position,
                                    event_type,
                                    row_index: 0,
                                },
                                &reason,
                            )?;
                            metadata.mark_table_needs_resync(
                                database_id,
                                &targets[target_index].source.name,
                                &reason,
                            )?;
                            blocked_targets.insert(target_index);
                        }
                        None => {}
                    }
                    position.pos = event_position;
                    if non_transactional && rows_event.flags().contains(RowsEventFlags::STMT_END) {
                        seal!();
                    }
                }
                EventData::XidEvent(_) => {
                    position.pos = event_position;
                    seal!();
                }
                EventData::QueryEvent(query) => {
                    let statement = query.query().into_owned();
                    let normalized = statement.trim().to_ascii_uppercase();
                    // MariaDB replaces each event a replica cannot read - the
                    // statement annotation before every row event, among
                    // others - with a query event holding only a comment,
                    // inside the transaction. It is not a statement, and
                    // treating it as one committed the open transaction
                    // statement by statement.
                    if normalized == "BEGIN" || normalized.starts_with('#') {
                        continue;
                    }
                    if normalized == "ROLLBACK" {
                        pending = PendingTransaction::default();
                    }
                    // A statement nobody can parse must not stop the
                    // database. Returning here aborts the pass, and the next
                    // pass resumes at the same offset and fails the same way:
                    // one unreadable DDL and that database never replicates
                    // again, which is how a source running ANSI_QUOTES took
                    // eighty-seven tables offline until an operator noticed.
                    //
                    // Skipping it outright would be worse in the other
                    // direction, because a schema change this missed leaves
                    // the replica quietly disagreeing with its source. So
                    // every tracked table the statement NAMES is quarantined
                    // for resync - a DDL that alters a table cannot avoid
                    // naming it, so this misses none, and over-quarantining
                    // costs a resync rather than a wrong answer - and the
                    // stream moves on.
                    let parsed = match parse_ddl(&statement, &report.database) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            pintail_log::log_error!(
                                "cdc unreadable ddl db={database_id} \
                                 quarantining the tables it names: {error}"
                            );
                            // The transactions before the statement are
                            // stored before anything it names is set aside.
                            flush!();
                            // Only tables the stream still follows: one a DROP took
                            // out stays in `targets`, and quarantining it as live
                            // hid that a CREATE of the same name makes a new table.
                            let named = targets
                                .iter()
                                .enumerate()
                                .filter(|(index, target)| {
                                    target_indexes.get(&target.source.name.to_ascii_lowercase())
                                        == Some(index)
                                        && statement_names_table(&statement, &target.source.name)
                                })
                                .map(|(index, _)| index)
                                .collect::<Vec<_>>();
                            let named_any = !named.is_empty();
                            for index in named {
                                quarantine_schema_change(
                                    &mut metadata,
                                    database_id,
                                    &targets[index],
                                    index,
                                    &mut blocked_targets,
                                    &statement,
                                    None,
                                )?;
                            }
                            // A CREATE of a table the stream does not track names
                            // nothing to quarantine, and skipping it left the new
                            // table uncopied for good. It is recorded as awaiting
                            // its first copy instead, as a copy that failed is,
                            // and the repair copies it from a fresh probe - which
                            // reads the source's own catalogue, not this text.
                            if !named_any
                                && let Some(table) =
                                    created_table_name(&statement, &report.database)
                            {
                                // A dropped table of the same name gives it up, as
                                // it does to a readable CREATE: left in place, the
                                // copy became readable but was never followed.
                                if let Some(root) = targets.iter().find_map(|target| {
                                    target.store.directory().parent().map(Path::to_path_buf)
                                }) {
                                    supersede_generation(
                                        &metadata,
                                        database_id,
                                        &root,
                                        &mut targets,
                                        &target_indexes,
                                        &table,
                                        true,
                                    )?;
                                }
                                metadata.upsert_snapshot_table(database_id, &table, None, None)?;
                                metadata.fail_table_copy(
                                    database_id,
                                    &table,
                                    &format!("its CREATE could not be read: {error}"),
                                    true,
                                )?;
                                pintail_log::log_error!(
                                    "table awaits its first copy db={database_id} table={table}: \
                                     its CREATE could not be read"
                                );
                            }
                            // Past it, as a readable DDL moves past itself:
                            // the rows before it commit, and the checkpoint
                            // names the statement's own position. Leaving
                            // both behind re-read the statement on every
                            // later pass - quarantining the same tables
                            // again - and held those rows until some other
                            // event arrived, which for the last event in the
                            // log is never.
                            position.pos = event_position;
                            seal!();
                            flush!();
                            continue;
                        }
                    };
                    // Session schema is the default routing; an explicit
                    // qualifier on the statement overrides it in both
                    // directions - `other_db.t` from a tracked session was
                    // already dropped during classification, and `tracked.t`
                    // from a foreign session is still this schema's DDL.
                    let tracks_schema = !parsed.actions.is_empty()
                        && (query.schema().is_empty()
                            || query.schema().eq_ignore_ascii_case(&report.database)
                            || parsed.names_tracked_schema);
                    let actions = parsed.actions;
                    if tracks_schema && pending.has_mutations() {
                        seal!();
                    }
                    if tracks_schema {
                        // A schema change is a batch boundary: every
                        // transaction before it is stored and checkpointed
                        // under the schema it was written with.
                        flush!();
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
                    seal!();
                    if tracks_schema {
                        // And the change itself is checkpointed before the
                        // next transaction is read, so a restart never
                        // applies it twice.
                        flush!();
                    }
                }
                // A connection replays its preamble before any replication
                // progress: a fake rotate, then the format description at the
                // head of the file. Their log_pos describes the FILE, not how
                // far this stream has read, so adopting it rewinds the resume
                // point to the beginning - observed as
                // "cdc cycle done ... events=2 commits=0 pos=mysql-bin.000003:127"
                // on every idle cycle. Nothing persists today because those
                // cycles commit nothing, but the safety rests on "a commit
                // cannot follow a preamble-only read", which nothing states
                // or tests. A checkpoint at 127 replays the whole binlog, and
                // on an append-keyed table that duplicates every row.
                EventData::FormatDescriptionEvent(_) => {}
                _ => {
                    if event_position > 0 {
                        position.pos = event_position;
                    }
                }
            }
            // Asked to stop, or out of time: honoured only on the event that
            // closed a transaction, so the position handed back never splits
            // one. The batch is written out there and then, whatever its
            // size, so whoever asked waits for one transaction and one
            // flush, not for a batch to fill.
            let closed = commits + batch.transactions;
            let yielding = closed > closed_before
                && !options.blocking
                && (options.stop.as_ref().is_some_and(CycleStop::requested)
                    || options
                        .max_duration
                        .is_some_and(|maximum| cycle_started.elapsed() >= maximum));
            let spent = options.max_commits.is_some_and(|maximum| closed >= maximum);
            if yielding || spent || batch.is_due(options.max_commits) {
                flush!();
            }
            if yielding || spent {
                stream.close().await?;
                pintail_log::log_debug!(
                    "cdc cycle done db={database_id} events={events_read} commits={commits} \
                     mutations={mutations} pos={}:{} {phases}",
                    position.file,
                    position.pos
                );
                settle_paused_skips(&metadata, database_id, &targets, &paused_skipped)?;
                return finish_result(
                    commits,
                    mutations,
                    &position,
                    targets,
                    options.stop.as_ref(),
                );
            }
        }
        // However the stream ended, the transactions it closed are stored
        // before anything rebuilds the position from the checkpoint.
        flush!();
        if let Some(error) = out_of_order {
            drop(stream);
            position = resnapshot_context
                .recover(
                    error,
                    &mut targets,
                    &mut blocked_targets,
                    &paused_targets,
                    &mut resnapshot_attempted,
                )
                .await?;
            pending = PendingTransaction::default();
            reconnect_attempts = 0;
            continue;
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
                            &paused_targets,
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
        // The one line that separates "the stream delivered nothing" from
        // "the stream delivered events this cycle declined to act on" - the
        // two are indistinguishable from outside, and a cycle that reads
        // zero events while the binlog is growing is a wedge that otherwise
        // logs nothing at all.
        pintail_log::log_debug!(
            "cdc cycle done db={database_id} events={events_read} commits={commits} \
             mutations={mutations} pos={}:{} {phases}",
            position.file,
            position.pos
        );
        settle_paused_skips(&metadata, database_id, &targets, &paused_skipped)?;
        return finish_result(
            commits,
            mutations,
            &position,
            targets,
            options.stop.as_ref(),
        );
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
        paused_targets: &BTreeSet<usize>,
        attempted: &mut bool,
    ) -> Result<StreamPosition, CdcError> {
        if !self.enabled || *attempted {
            return Err(error);
        }
        // An error, not a lifecycle note: the stream lost its place and every
        // table is about to be copied again. That is what an operator
        // needs paged about, whatever the source did to cause it.
        pintail_log::log_error!(
            "cdc.resnapshot db={} rebuilding after unavailable source position: {error}",
            self.database_id
        );
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
        pintail_failpoint::hit("cdc.resnapshot.after_targets").map_err(|source| {
            StoreError::Io {
                action: "recovery failpoint".to_owned(),
                source,
            }
        })?;
        let checkpoint = MetaStore::open(self.metadata_path)?
            .snapshot_checkpoint(self.database_id)?
            .ok_or_else(|| {
                CdcError::InvalidCheckpoint(
                    "automatic resnapshot did not capture a source position".to_owned(),
                )
            })?;
        // The quarantines this recovery repaired are gone, but a pause is
        // an operator's decision and the recopy did not lift it: without
        // restoring it the stream would apply changes to a table the
        // operator holds still, and then flag it for a second recopy.
        blocked_targets.clear();
        blocked_targets.extend(paused_targets.iter().copied());
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
    // The copy below rebuilds every target from the source as it IS, not as
    // it was when the stream started. The position was usually lost because
    // binlogs were purged, and purged binlogs may contain DDL this stream
    // never saw - copying with the remembered column list dies on the
    // source's own "Unknown column" error, and the recovery that exists to
    // unstick the stream becomes the thing that keeps it stuck.
    let refreshed = probe_source(pool, &report.database).await?;
    let mut metadata = MetaStore::open(metadata_path)?;
    let mut snapshot_targets = Vec::with_capacity(targets.len());
    for mut target in targets {
        if let Some(fresh) = find_source_table(&refreshed, &target.source.name) {
            let fresh = pintail_probe::stabilize_source_table(&target.source, fresh.clone())
                .map_err(CdcError::Ddl)?;
            if fresh.columns != target.source.columns {
                let version = next_schema_version(target.store.schema().version())?;
                evolve_tracked_schema(
                    &mut metadata,
                    database_id,
                    &target.source.name,
                    &mut target.store,
                    &fresh.columns,
                    fresh.table_schema_with_version(version)?,
                    None,
                )??;
            }
            target.source = fresh;
        }
        target.store.reset_for_resnapshot()?;
        snapshot_targets.push(SnapshotTarget::new(target.source, target.store)?);
    }
    metadata.begin_resnapshot(database_id, &Utc::now().to_rfc3339())?;
    drop(metadata);
    let snapshot = match run_snapshot(
        pool,
        metadata_path,
        database_id,
        &refreshed,
        snapshot_targets,
        snapshot_options,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // The snapshot captured a new source position before copying.
            // If its connection then fails, a later CDC cycle can connect
            // at that position even though some stores were reset and never
            // refilled. Flag those copies now, while the process is alive;
            // the supervisor's boot-only sweep cannot repair this window.
            let metadata = MetaStore::open(metadata_path)?;
            let reason = format!("automatic resnapshot interrupted: {error}");
            for name in metadata.tables_without_complete_copy(database_id)? {
                metadata.mark_table_needs_resync(database_id, &name, &reason)?;
            }
            return Err(error.into());
        }
    };
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
    // A queue rather than a plain loop: renaming an untracked table into the
    // schema is handled as the creation of its new name, which runs after the
    // rename in the same statement's order.
    let mut queue = VecDeque::from(actions);
    while let Some(action) = queue.pop_front() {
        match action {
            DdlAction::Alter {
                table,
                kind: AlterKind::AddOrDropColumns { added, dropped },
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
                apply_column_change(
                    metadata,
                    database_id,
                    targets,
                    index,
                    blocked_targets,
                    statement,
                    source,
                    (added.as_slice(), dropped.as_slice()),
                )?;
            }
            DdlAction::Alter {
                table,
                kind: AlterKind::RenameTable { new_name },
            } => {
                let Some(&index) = target_indexes.get(&table.to_ascii_lowercase()) else {
                    // The old name was never mirrored - typically a table
                    // created and filled under a staging name and swapped in
                    // - so the new name is a table this schema has not seen.
                    queue.push_front(DdlAction::Create { table: new_name });
                    continue;
                };
                if target_indexes.contains_key(&new_name.to_ascii_lowercase()) {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        &format!("{statement}; {new_name} is already tracked"),
                        None,
                    )?;
                    continue;
                }
                // A dropped table still holding the new name gives it up, as
                // it does to a CREATE; the renamed table then takes its row.
                if let Some(root) = targets[index]
                    .store
                    .directory()
                    .parent()
                    .map(Path::to_path_buf)
                    && let Some(orphan) = supersede_generation(
                        metadata,
                        database_id,
                        &root,
                        targets,
                        target_indexes,
                        &new_name,
                        true,
                    )?
                {
                    metadata.remove_local_table(database_id, &orphan)?;
                }
                // Metadata first, then the directory: a crash between the
                // two leaves a row whose directory is missing, which the
                // restart sweep flags for resync; the reverse would leave a
                // directory no row names.
                metadata.rename_table(database_id, &table, &new_name)?;
                let root = targets[index]
                    .store
                    .directory()
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| CdcError::Ddl("table directory has no parent".to_owned()))?;
                let new_directory = pintail_store::table_directory(&root, &new_name);
                if let Err(error) = targets[index].store.rename_directory(&new_directory) {
                    quarantine_schema_change(
                        metadata,
                        database_id,
                        &targets[index],
                        index,
                        blocked_targets,
                        &format!("{statement}; {error}"),
                        None,
                    )?;
                    continue;
                }
                target_indexes.remove(&table.to_ascii_lowercase());
                target_indexes.insert(new_name.to_ascii_lowercase(), index);
                targets[index].source.name = new_name;
                // The replica lists tables from the stored probe report;
                // the refreshed report already carries the new name, so
                // storing it leaves no window in which the table is absent.
                // Best effort: the next probe stores the same.
                if let Ok(Some(database)) = metadata.database(database_id)
                    && let Some(mode) = database.effective_mode.as_deref()
                    && let Ok(json) = serde_json::to_string(&refreshed)
                {
                    let _ = metadata.update_database_probe(
                        database_id,
                        &json,
                        mode,
                        &Utc::now().to_rfc3339(),
                    );
                }
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
                let source = match pintail_probe::stabilize_source_table(&previous, source) {
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
                let name = targets[index].source.name.clone();
                if let Err(error) = evolve_tracked_schema(
                    metadata,
                    database_id,
                    &name,
                    &mut targets[index].store,
                    &source.columns,
                    schema,
                    Some(statement),
                )? {
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
                let source =
                    match pintail_probe::stabilize_source_table(&targets[index].source, source) {
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
                let name = targets[index].source.name.clone();
                if let Err(error) = evolve_tracked_schema(
                    metadata,
                    database_id,
                    &name,
                    &mut targets[index].store,
                    &source.columns,
                    schema,
                    Some(statement),
                )? {
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
                match pintail_probe::stabilize_source_table(&targets[index].source, source) {
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
                truncate_target(metadata, database_id, &mut targets[index], statement)?;
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
                // The retained rows stay readable, but the name no longer
                // belongs to them: a CREATE or RENAME that reuses it later in
                // this run is a different table.
                target_indexes.remove(&table.to_ascii_lowercase());
                snapshot_fences.remove(&index);
            }
            DdlAction::Create { table } => {
                if !options.auto_include_new_tables
                    || !new_table_matches(&table, options)
                    || target_indexes.contains_key(&table.to_ascii_lowercase())
                {
                    continue;
                }
                // The probe reads the source as it is now, not as it was at
                // this event. A table created and then dropped or renamed
                // before this cycle replayed its CREATE is absent, and there
                // is nothing to mirror under this name: its row events are
                // for an untracked table, and a later rename reaches this arm
                // again under the name that does exist. Failing here instead
                // left the checkpoint before the CREATE, so every cycle
                // replayed it and failed the same way.
                let Some(source) = find_source_table(&refreshed, &table).cloned() else {
                    continue;
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
                supersede_generation(
                    metadata,
                    database_id,
                    &root,
                    targets,
                    target_indexes,
                    &table,
                    true,
                )?;
                let directory = new_table_directory(&root, &table);
                let store = match TableStore::open(
                    &directory,
                    source.table_schema()?,
                    StoreOptions::default(),
                ) {
                    Ok(store) => store,
                    // Files under the name that cannot take the new table's
                    // shape belong to a generation nothing marked as dropped
                    // - a table dropped while it was being recopied, whose
                    // DROP no stream saw. The CREATE is proof they are stale.
                    Err(error) if superseded_layout(&error) => {
                        supersede_generation(
                            metadata,
                            database_id,
                            &root,
                            targets,
                            target_indexes,
                            &table,
                            false,
                        )?;
                        TableStore::open(
                            directory,
                            source.table_schema()?,
                            StoreOptions::default(),
                        )?
                    }
                    Err(error) => return Err(error.into()),
                };
                let snapshot_target = SnapshotTarget::new(source.clone(), store)?;
                let snapshot = match run_snapshot(
                    pool,
                    metadata_path,
                    database_id,
                    &refreshed,
                    vec![snapshot_target],
                    options.resnapshot_options.clone(),
                )
                .await
                {
                    Ok(snapshot) => snapshot,
                    // A lock the copy could not take, or a source busy enough
                    // to refuse it, belongs to this table alone. Returning it
                    // left the checkpoint before the CREATE, so every cycle
                    // re-ran the copy and failed every table with it. The
                    // table is recorded as awaiting its copy instead, and the
                    // stream moves past the CREATE: its row events are
                    // skipped until the repair copies it.
                    Err(error) => {
                        let reason = format!("its first copy failed: {error}");
                        let key_json = serde_json::to_string(&source.key.columns)
                            .map_err(|error| CdcError::Ddl(error.to_string()))?;
                        metadata.upsert_snapshot_table(
                            database_id,
                            &source.name,
                            Some(&key_json),
                            Some(&key_json),
                        )?;
                        metadata.fail_table_copy(database_id, &source.name, &reason, true)?;
                        let probe_json = serde_json::to_string(&refreshed)
                            .map_err(|error| CdcError::Ddl(error.to_string()))?;
                        metadata.refresh_database_probe_json(
                            database_id,
                            &probe_json,
                            &Utc::now().to_rfc3339(),
                        )?;
                        pintail_log::log_error!(
                            "table quarantined db={database_id} table={}: {reason}",
                            source.name
                        );
                        continue;
                    }
                };
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
///
/// Public because a single-table resnapshot is performed outside this crate
/// and must record its fence under the same key the stream reads, or the
/// events it just copied replay over it.
#[must_use]
pub fn snapshot_fence_key(database_id: &str, table: &str) -> String {
    format!("cdc_snapshot_fence:{database_id}:{table}")
}

fn fence_key(database_id: &str, table: &str) -> String {
    snapshot_fence_key(database_id, table)
}

fn find_source_table<'a>(report: &'a ProbeReport, table: &str) -> Option<&'a SourceTable> {
    report
        .tables
        .iter()
        .find(|source| source.name.eq_ignore_ascii_case(table))
}

/// Whether a statement names a table, ignoring case and any quoting around
/// it.
///
/// Used only when a statement could not be parsed, to decide which tracked
/// tables to quarantine. It errs toward saying yes: a name appearing in a
/// comment or a value quarantines a table that did not change, which costs a
/// resync, where missing one would leave the replica disagreeing with its
/// source and nobody the wiser.
/// The table a `CREATE TABLE` statement creates in `database`, read from the
/// statement's head without parsing the rest - for a CREATE the parser
/// rejects. `None` for any other statement, a temporary table (which row
/// events never carry), or a table in another schema.
fn created_table_name(statement: &str, database: &str) -> Option<String> {
    fn word<'a>(text: &mut &'a str) -> Option<&'a str> {
        *text = text.trim_start();
        let end = text
            .find(|ch: char| ch.is_whitespace() || ch == '(')
            .unwrap_or(text.len());
        let (head, tail) = text.split_at(end);
        *text = tail;
        (!head.is_empty()).then_some(head)
    }
    fn identifier(text: &str) -> String {
        text.strip_prefix('`')
            .and_then(|inner| inner.strip_suffix('`'))
            .map_or_else(|| text.to_owned(), |inner| inner.replace("``", "`"))
    }
    let mut rest = statement;
    if !word(&mut rest)?.eq_ignore_ascii_case("create") {
        return None;
    }
    let mut next = word(&mut rest)?;
    if next.eq_ignore_ascii_case("temporary") {
        return None;
    }
    if !next.eq_ignore_ascii_case("table") {
        return None;
    }
    next = word(&mut rest)?;
    if next.eq_ignore_ascii_case("if") {
        word(&mut rest)?;
        word(&mut rest)?;
        next = word(&mut rest)?;
    }
    // `schema.table`, either part optionally quoted; a quoted part may hold
    // a dot, so the split is at a dot outside backticks.
    let mut quoted = false;
    let split = next.char_indices().find_map(|(at, ch)| {
        if ch == '`' {
            quoted = !quoted;
        }
        (ch == '.' && !quoted).then_some(at)
    });
    let table = match split {
        Some(at) => {
            if !identifier(&next[..at]).eq_ignore_ascii_case(database) {
                return None;
            }
            identifier(&next[at + 1..])
        }
        None => identifier(next),
    };
    (!table.is_empty()).then_some(table)
}

fn statement_names_table(statement: &str, table: &str) -> bool {
    if table.is_empty() {
        return false;
    }
    let statement = statement.to_ascii_lowercase();
    let table = table.to_ascii_lowercase();
    statement.match_indices(&table).any(|(at, _)| {
        let before = statement[..at].chars().next_back();
        let after = statement[at + table.len()..].chars().next();
        let boundary =
            |ch: Option<char>| ch.is_none_or(|ch| !ch.is_alphanumeric() && ch != '_' && ch != '$');
        boundary(before) && boundary(after)
    })
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

/// Records a table's first generation - the shape its store was just opened
/// with - when schema history has none.
///
/// Without that row the shape of an unaltered table was read from the stored
/// probe on every open, and several paths rewrite the probe with the
/// source's CURRENT shape: a re-probe, a forced snapshot, a sibling table's
/// resync. A source ALTER not yet streamed then redefined version 1 under a
/// store still holding the old columns, and the store refused to open with
/// a fingerprint mismatch. Frozen here, version 1 stays what is on disk, and
/// the ALTER, when it streams, evolves it like any other.
///
/// # Errors
///
/// Returns an error when the columns cannot be serialized or the history row
/// cannot be written.
pub fn freeze_first_generation(
    metadata: &mut MetaStore,
    database_id: &str,
    source: &SourceTable,
) -> Result<(), CdcError> {
    let columns_json =
        serde_json::to_string(&source.columns).map_err(|error| CdcError::Ddl(error.to_string()))?;
    metadata.record_first_schema_generation(
        database_id,
        &source.name,
        &columns_json,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

/// Evolves a tracked table's storage to `schema`, recording the generation in
/// its schema history first.
///
/// The order is the crash contract. A tracked table reopens with its latest
/// recorded schema; an open with a schema newer than the store's manifest
/// upgrades in place, and one older than the manifest cannot open at all.
/// Recording first means a death between the two steps leaves the history one
/// generation ahead, which the next open rolls forward. The other order left
/// storage a generation ahead of any schema that says how to read it, and the
/// table could not be opened again. A store that refuses the schema takes the
/// record back, so the history never names a generation no store took.
///
/// The outer error is metadata failing; the inner one is the store refusing
/// the schema, which callers quarantine.
///
/// # Errors
///
/// Returns an error when the history cannot be written or taken back.
pub fn evolve_tracked_schema(
    metadata: &mut MetaStore,
    database_id: &str,
    table_name: &str,
    store: &mut TableStore,
    columns: &[pintail_probe::SourceColumn],
    schema: pintail_types::TableSchema,
    statement: Option<&str>,
) -> Result<Result<(), StoreError>, CdcError> {
    let version = schema.version();
    let columns_json =
        serde_json::to_string(columns).map_err(|error| CdcError::Ddl(error.to_string()))?;
    metadata.record_schema_history(
        database_id,
        table_name,
        version,
        statement,
        &columns_json,
        &Utc::now().to_rfc3339(),
    )?;
    recovery_point("cdc.ddl.after_history")?;
    if let Err(error) = store.evolve_schema(schema) {
        metadata.forget_schema_version(database_id, table_name, version)?;
        return Ok(Err(error));
    }
    recovery_point("cdc.ddl.after_evolve")?;
    Ok(Ok(()))
}

/// Adopts a column-level schema change for one tracked table, given the
/// table as the source now declares it. A shape the tracked table cannot
/// take without a copy quarantines the table instead.
#[allow(clippy::too_many_arguments)]
fn apply_column_change(
    metadata: &mut MetaStore,
    database_id: &str,
    targets: &mut [CdcTarget],
    index: usize,
    blocked_targets: &mut BTreeSet<usize>,
    statement: &str,
    source: SourceTable,
    (added, dropped): (&[String], &[String]),
) -> Result<(), CdcError> {
    // The probe reads the source as it is NOW, which can be past this
    // statement: a column dropped and then added back under its name reads
    // as never having left, and the in-place change would keep the dropped
    // column's values for the new one. Evolve in place only when the source
    // still shows exactly what this statement did.
    let named = |columns: &[pintail_probe::SourceColumn], name: &str| {
        columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(name))
    };
    if dropped.iter().any(|name| named(&source.columns, name))
        || added.iter().any(|name| {
            !named(&source.columns, name) || named(&targets[index].source.columns, name)
        })
    {
        return quarantine_schema_change(
            metadata,
            database_id,
            &targets[index],
            index,
            blocked_targets,
            &format!(
                "{statement}; the source's schema has already moved past this statement, \
                 so the table is recopied instead of evolved in place"
            ),
            None,
        );
    }
    let source = match pintail_probe::stabilize_source_table(&targets[index].source, source) {
        Ok(source) => source,
        Err(reason) => {
            return quarantine_schema_change(
                metadata,
                database_id,
                &targets[index],
                index,
                blocked_targets,
                &format!("{statement}; {reason}"),
                None,
            );
        }
    };
    let source = renumber_readded_columns(metadata, database_id, &targets[index].source, source)?;
    if let Some(reason) = added_column_needs_values(&targets[index].source, &source) {
        return quarantine_schema_change(
            metadata,
            database_id,
            &targets[index],
            index,
            blocked_targets,
            &format!("{statement}; {reason}"),
            Some(&source),
        );
    }
    let version = next_schema_version(targets[index].store.schema().version())?;
    let schema = source.table_schema_with_version(version)?;
    let name = targets[index].source.name.clone();
    if let Err(error) = evolve_tracked_schema(
        metadata,
        database_id,
        &name,
        &mut targets[index].store,
        &source.columns,
        schema,
        Some(statement),
    )? {
        return quarantine_schema_change(
            metadata,
            database_id,
            &targets[index],
            index,
            blocked_targets,
            &format!("{statement}; {error}"),
            Some(&source),
        );
    }
    targets[index].source = source;
    Ok(())
}

/// Empties a tracked table for a TRUNCATE: a new schema generation with the
/// same columns, then a reset of its storage.
fn truncate_target(
    metadata: &mut MetaStore,
    database_id: &str,
    target: &mut CdcTarget,
    statement: &str,
) -> Result<(), CdcError> {
    let version = next_schema_version(target.store.schema().version())?;
    let schema = target.source.table_schema_with_version(version)?;
    evolve_tracked_schema(
        metadata,
        database_id,
        &target.source.name,
        &mut target.store,
        &target.source.columns,
        schema,
        Some(statement),
    )?
    .and_then(|()| target.store.reset_for_resnapshot())
    .map_err(|error| CdcError::from(error).for_table(&target.source.name))
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

/// Makes room for a source table under a dropped table's name.
///
/// Dropped tables keep serving their last rows until the name is reused.
/// Every identity in the mirror - the table row, its schema history, its
/// fence and its directory - is keyed by the name, so a new table under it
/// was either skipped in favour of the old rows, which then answered queries
/// as if they were the new table, or opened over the old generation's files
/// and refused, which stopped the whole database. The orphan's metadata and
/// files are removed first; a store still open from a drop earlier in this
/// run is moved aside before its files go, so no handle outlives its
/// directory under the reused name. With `orphaned_only` unset the files go
/// whether or not a row marks them dropped. Returns the table row's stored
/// name when one was reset.
///
/// The files go first and the metadata commit last, and that order is the
/// crash contract. Committed first, a crash in between leaves a table row
/// that is live again beside the old generation's files: the next cycle
/// opens them against the new table's shape, is refused, and change capture
/// stops for every table in the database, with nothing to retry it. This
/// way a crash leaves the row still marked dropped and the files gone - the
/// retained rows of an already-dropped table, which the replayed CREATE
/// supersedes again on the next pass.
fn supersede_generation(
    metadata: &MetaStore,
    database_id: &str,
    root: &Path,
    targets: &mut [CdcTarget],
    target_indexes: &BTreeMap<String, usize>,
    table: &str,
    orphaned_only: bool,
) -> Result<Option<String>, CdcError> {
    let stored_name = metadata.superseded_table_name(database_id, table, orphaned_only)?;
    if orphaned_only && stored_name.is_none() {
        return Ok(None);
    }
    let directory = new_table_directory(root, stored_name.as_deref().unwrap_or(table));
    let tracked = target_indexes.values().copied().collect::<BTreeSet<_>>();
    let canonical = std::fs::canonicalize(&directory).ok();
    for (index, target) in targets.iter_mut().enumerate() {
        if tracked.contains(&index) || Some(target.store.directory()) != canonical.as_deref() {
            continue;
        }
        let retired = root.join(format!(
            ".superseded-{}-{}",
            index,
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        target.store.rename_directory(&retired)?;
        std::fs::remove_dir_all(&retired).map_err(|source| StoreError::Io {
            action: "remove a superseded table directory".to_owned(),
            source,
        })?;
        pintail_store::publish_changes_under(&retired);
    }
    if directory.exists() {
        std::fs::remove_dir_all(&directory).map_err(|source| StoreError::Io {
            action: "remove a superseded table directory".to_owned(),
            source,
        })?;
    }
    pintail_store::publish_changes_under(&directory);
    // The removals have to outlive a power loss, not only a process crash,
    // or the metadata commit below can be the only half that survives.
    if root.exists() {
        pintail_store::sync_directory(root)?;
    }
    let reset = metadata.supersede_table_generation(database_id, table, orphaned_only)?;
    Ok(reset)
}

/// Whether a store refused to open because its files were written for a
/// different table shape, rather than because they are damaged.
fn superseded_layout(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::SchemaMismatch { .. }
            | StoreError::SchemaFingerprintMismatch { .. }
            | StoreError::IncompatibleSchema(_)
    )
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

fn stored_version_floor(targets: &[CdcTarget]) -> u64 {
    targets
        .iter()
        .filter_map(|target| target.store.snapshot().max_row_version())
        .max()
        .unwrap_or(0)
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
    // Logged before the sleep, so a mirror stuck in backoff shows why while
    // it is still happening rather than only after it gives up.
    pintail_log::log_error!(
        "cdc reconnect db={database_id} attempt={attempts} delay={}ms reason={error}",
        delay.as_millis(),
        attempts = *attempts
    );
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
    // A run with no targets still has work: the CREATE events that adopt new
    // tables. Refusing it is what strands a database whose tracked tables
    // were all dropped and re-created - the dropped rows are retained rather
    // than streamed, so there is nothing to open, and without a run nothing
    // ever reads the CREATEs that would supersede them. The retained rows
    // would be served as the live table for good.
    if targets.is_empty() && !(options.auto_include_new_tables && options.new_table_root.is_some())
    {
        return Err(CdcError::InvalidConfiguration(
            "CDC requires at least one target, or a root under which to adopt new tables"
                .to_owned(),
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
    /// Buffered: the encoder writes a token at a time and the decoder reads
    /// a byte at a time, and against the bare file each of those was a
    /// system call - a million-row transaction spent minutes in them.
    spill: Option<BufWriter<File>>,
    spilled_mutations: usize,
    discarded_targets: BTreeSet<usize>,
    retained_bytes: usize,
    ordinal: u32,
    /// Targets whose snapshot fence this transaction's events are past.
    passed_fences: BTreeSet<usize>,
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
            self.spill = Some(BufWriter::with_capacity(
                1 << 20,
                tempfile::tempfile()
                    .map_err(|error| CdcError::TransactionSpill(error.to_string()))?,
            ));
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

    /// Hands the transaction's mutations to `each` in the order they were
    /// staged, reading a spilled transaction back one row at a time: it
    /// spilled because it does not fit in memory, and reading it back whole
    /// at its commit held it there anyway.
    fn for_each_mutation(
        &mut self,
        mut each: impl FnMut(PendingMutation) -> Result<(), CdcError>,
    ) -> Result<(), CdcError> {
        if let Some(spill) = &mut self.spill {
            spill
                .flush()
                .and_then(|()| spill.get_mut().rewind())
                .map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
            let file = BufReader::with_capacity(1 << 20, spill.get_mut());
            for mutation in
                serde_json::Deserializer::from_reader(file).into_iter::<PendingMutation>()
            {
                let mutation =
                    mutation.map_err(|error| CdcError::TransactionSpill(error.to_string()))?;
                if !self.discarded_targets.contains(&mutation.target_index) {
                    each(mutation)?;
                }
            }
        }
        for mutation in std::mem::take(&mut self.mutations) {
            if !self.discarded_targets.contains(&mutation.target_index) {
                each(mutation)?;
            }
        }
        Ok(())
    }
}

/// Reconciles a table whose binlog row image has more or fewer columns than
/// the probed schema, without resnapshotting it.
///
/// The DDL stream is the normal way a schema change arrives, and when it does
/// this never fires. But that path has single points of failure - the
/// statement landing while the stream was disconnected, a form the parser
/// cannot classify, a topology where the DDL never reaches this stream - and
/// every one of them shows up here instead, as a row image the decoder
/// refuses because it cannot know which column is which. Production hit this
/// three days running on a hand-written `ALTER TABLE ... ADD COLUMN`, and
/// each occurrence marked the table for a full resnapshot: the whole table
/// re-copied because one column appeared.
///
/// Re-probing and adopting the refreshed schema costs one round trip and
/// keeps the stream running. It succeeds exactly when `stabilize_source_table`
/// says the change is storage-compatible - an added or dropped column, which
/// is the common case - and when it does not, the caller's existing path
/// quarantines for resync as before. Healing is therefore strictly a
/// shortcut around work that would otherwise happen anyway.
async fn heal_schema_drift(
    pool: &Pool,
    database: &str,
    database_id: &str,
    target: &mut CdcTarget,
    metadata: &mut MetaStore,
    table_map: &mysql_async::binlog::events::TableMapEvent<'_>,
) {
    let table = target.source.name.clone();
    let previous = target.source.columns.len();
    let row_columns = usize::try_from(table_map.columns_count()).unwrap_or(usize::MAX);
    match adopt_drifted_schema(pool, database, database_id, target, metadata, table_map).await {
        Ok(()) => {
            pintail_log::log_info!(
                "cdc drift healed db={database_id} table={table} schema={previous}->{} for a \
                 {row_columns}-column row image",
                target.source.columns.len()
            );
        }
        Err(reason) => {
            // A declined heal ends in quarantine, and the operator's first
            // question is which of the several ways it could decline actually
            // fired. Saying so here costs one line per drift.
            pintail_log::log_error!(
                "cdc drift declined db={database_id} table={table} schema={previous} row image \
                 {row_columns}: {reason}"
            );
        }
    }
}

/// A `VIRTUAL` generated column joining a schema cannot evolve in place: the
/// rows already copied would read NULL where the source computes a value
/// for every one of them. Returns the reason the table has to be recopied.
/// Gives every column new to `previous` an ID no generation of the table has
/// used. Stable IDs continue from the highest of the columns the table has
/// now, so a column dropped and then added again under its old name got the
/// dropped one's ID back - and older segments still hold the dropped values
/// under it, which the new column then read. The schema history records every
/// generation's columns, so its highest ID is the floor.
fn renumber_readded_columns(
    metadata: &MetaStore,
    database_id: &str,
    previous: &SourceTable,
    mut source: SourceTable,
) -> Result<SourceTable, CdcError> {
    let floor = metadata
        .schema_history(database_id, &previous.name)?
        .iter()
        .filter_map(|record| {
            serde_json::from_str::<Vec<pintail_probe::SourceColumn>>(&record.columns_json).ok()
        })
        .flatten()
        .map(|column| column.id)
        .chain(previous.columns.iter().map(|column| column.id))
        .max()
        .unwrap_or(0);
    let mut next_id = floor;
    for column in &mut source.columns {
        let known = previous
            .columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&column.name));
        if !known && column.id <= floor {
            next_id = next_id
                .checked_add(1)
                .ok_or_else(|| CdcError::Ddl("stable column ID space is exhausted".to_owned()))?;
            column.id = next_id;
        }
    }
    Ok(source)
}

/// A column that joins the schema with values the stream cannot supply for
/// the rows already copied: a VIRTUAL generated column, or one with a
/// default. The source filled its default into every existing row - a literal
/// or an expression evaluated when the ALTER ran - but an ALTER carries no row
/// events, so evolved in place those rows would read NULL. The table is
/// recopied instead. A nullable column with no default needs nothing.
fn added_column_needs_values(previous: &SourceTable, refreshed: &SourceTable) -> Option<String> {
    refreshed
        .columns
        .iter()
        .find(|column| {
            (column.virtual_generated()
                || column.default_value.is_some()
                || column.default_generated)
                && !previous
                    .columns
                    .iter()
                    .any(|known| known.name.eq_ignore_ascii_case(&column.name))
        })
        .map(|added| {
            format!(
                "column {} joined the schema with values the rows already copied need; the \
                 table is recopied instead of evolved in place",
                added.name
            )
        })
}

async fn adopt_drifted_schema(
    pool: &Pool,
    database: &str,
    database_id: &str,
    target: &mut CdcTarget,
    metadata: &mut MetaStore,
    table_map: &mysql_async::binlog::events::TableMapEvent<'_>,
) -> Result<(), String> {
    let refreshed = probe_source(pool, database)
        .await
        .map_err(|error| format!("re-probe failed: {error}"))?;
    let source = find_source_table(&refreshed, &target.source.name)
        .cloned()
        .ok_or_else(|| "table is absent from the refreshed probe".to_owned())?;
    let source = pintail_probe::stabilize_source_table(&target.source, source)?;
    let source = renumber_readded_columns(metadata, database_id, &target.source, source)
        .map_err(|error| error.to_string())?;
    // Declining leaves the event to the quarantine path, and the resync
    // that follows copies the column's values with the refreshed schema.
    if let Some(reason) = added_column_needs_values(&target.source, &source) {
        return Err(reason);
    }
    // Only adopt a schema that actually explains the row in hand. A probe the
    // row still cannot be placed against means the drift is something else - a
    // rename, a table swapped underneath - and guessing would silently corrupt
    // column identity, which is worse than the resync this declines into.
    RowAlignment::resolve(&source, table_map, UnknownColumn::Ignore)
        .map_err(|error| error.to_string())?;
    // From here the work is exactly what the DDL path performs, because the
    // outcome has to be indistinguishable from having seen the statement:
    // carrying the new column list on the source alone would keep the stream
    // decoding while every query still resolved against the old schema.
    let version =
        next_schema_version(target.store.schema().version()).map_err(|error| error.to_string())?;
    let schema = source
        .table_schema_with_version(version)
        .map_err(|error| error.to_string())?;
    // The history has no statement to quote - that is the whole point of this
    // path - so it records how the change was learned instead.
    let statement = format!(
        "-- schema drift adopted from source probe ({} columns)",
        source.columns.len()
    );
    evolve_tracked_schema(
        metadata,
        database_id,
        &source.name,
        &mut target.store,
        &source.columns,
        schema,
        Some(&statement),
    )
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    target.source = source;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_rows_event(
    rows_event: &RowsEventData<'_>,
    table_map: &mysql_async::binlog::events::TableMapEvent<'_>,
    source: &SourceTable,
    alignment: &RowAlignment,
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
    let columns = usize::try_from(rows_event.num_columns()).unwrap_or(usize::MAX);
    let before_present = rows_event
        .columns_before_image()
        .map_or_else(Vec::new, |bits| {
            image_ordinals(bits.iter().map(|bit| *bit), columns)
        });
    let after_present = rows_event
        .columns_after_image()
        .map_or_else(Vec::new, |bits| {
            image_ordinals(bits.iter().map(|bit| *bit), columns)
        });
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
            alignment,
            target_index,
            row,
            (&before_present, &after_present),
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

#[allow(clippy::too_many_arguments)]
fn decode_row_pair(
    source: &SourceTable,
    alignment: &RowAlignment,
    target_index: usize,
    (before, after): (Option<BinlogRow>, Option<BinlogRow>),
    (before_present, after_present): (&[usize], &[usize]),
    position: &StreamPosition,
    event_position: u64,
    pending: &mut PendingTransaction,
    maximum_bytes: usize,
) -> Result<(), CdcError> {
    // A keyless table refuses before decoding: the refusal must not depend on
    // whether the image happens to decode.
    if before.is_some() && source.key.mode == KeyMode::AppendRowId {
        return Err(keyless_change_error(source, after.is_some()));
    }
    let before = before
        .map(|row| decode_row(source, row, alignment, before_present))
        .transpose()?;
    let after = after
        .map(|row| decode_row(source, row, alignment, after_present))
        .transpose()?;
    stage_row_change(
        source,
        target_index,
        (before, after),
        position,
        event_position,
        pending,
        maximum_bytes,
    )
}

/// Turns one decoded row change into versioned store mutations on the open
/// transaction: an insert is a live row, a delete a tombstone at the old key,
/// and an update that moves the key a tombstone followed by the new row at
/// the next ordinal. Versions come from the stream position alone, so
/// replaying the same events stages the same mutations.
fn stage_row_change(
    source: &SourceTable,
    target_index: usize,
    (before, after): (Option<Vec<Value>>, Option<Vec<Value>>),
    position: &StreamPosition,
    event_position: u64,
    pending: &mut PendingTransaction,
    maximum_bytes: usize,
) -> Result<(), CdcError> {
    match (before, after) {
        (None, Some(values)) => {
            let version = position.version(event_position, pending.ordinal)?;
            let key = insert_key(source, &values, version)?;
            push_mutations(
                pending,
                vec![PendingMutation {
                    target_index,
                    row: StoredRow::new(key, values, version, false),
                }],
                maximum_bytes,
                position.ordinal_budget(),
            )
        }
        (Some(values), None) => {
            if source.key.mode == KeyMode::AppendRowId {
                return Err(keyless_change_error(source, false));
            }
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
                position.ordinal_budget(),
            )
        }
        (Some(before_values), Some(after_values)) => {
            if source.key.mode == KeyMode::AppendRowId {
                return Err(keyless_change_error(source, true));
            }
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
            push_mutations(pending, mutations, maximum_bytes, position.ordinal_budget())
        }
        (None, None) => Err(CdcError::Decode(
            "row event contains neither before nor after image".to_owned(),
        )),
    }
}

fn keyless_change_error(source: &SourceTable, update: bool) -> CdcError {
    CdcError::Decode(format!(
        "{} {} has no stable source key and requires resnapshot",
        source.name,
        if update { "UPDATE" } else { "DELETE" }
    ))
}

fn push_mutations(
    pending: &mut PendingTransaction,
    mutations: Vec<PendingMutation>,
    maximum_bytes: usize,
    ordinal_budget: u32,
) -> Result<(), CdcError> {
    let mutation_count = u32::try_from(mutations.len())
        .map_err(|error| CdcError::Decode(format!("mutation count conversion failed: {error}")))?;
    let next_ordinal = pending
        .ordinal
        .checked_add(mutation_count)
        .ok_or_else(|| CdcError::Decode("mutation ordinal overflowed".to_owned()))?;
    // This gate fired at u16::MAX regardless of mode even after the GTID
    // version layout grew its 24-bit ordinal - the browser soak's 65,536-row
    // transaction quarantined HERE while version() stood ready to encode it.
    if next_ordinal > ordinal_budget {
        return Err(CdcError::Decode(format!(
            "one source transaction exceeds {ordinal_budget} row mutations"
        )));
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

/// Most source transactions one batch carries.
const BATCH_TRANSACTIONS: usize = 16_384;
/// Row bytes a batch carries before it is written out whatever its age. A
/// table's share of a batch is one WAL record, and this keeps a batch of
/// small transactions well under the record limit.
const BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Longest a closed transaction waits in the batch while the stream keeps
/// delivering. It bounds how stale a query can be on a mirror that is
/// behind; one that has caught up flushes as soon as the stream goes quiet.
const BATCH_AGE: Duration = Duration::from_millis(250);
/// Row bytes of a spilled transaction held in memory while it is stored.
/// Past this, the table holding the most is written out as segments its
/// readers do not see until the transaction's batch is stored.
const STAGE_BYTES: usize = 32 * 1024 * 1024;
/// Threads one flush spreads its tables over.
const FLUSH_WORKERS: usize = 8;
/// Rows below which a flush writes its tables one after another: starting
/// threads costs more than encoding a few rows.
const PARALLEL_INGEST_ROWS: usize = 512;

/// Whole source transactions closed by the stream and not yet stored.
///
/// A transaction enters only at its commit, so the batch never holds part of
/// one, and each table receives its share as a single WAL record: a reader
/// sees every transaction of the batch on that table or none of them.
#[derive(Default)]
struct ApplyBatch {
    /// Rows per target, in source commit order.
    rows: BTreeMap<usize, Vec<StoredRow>>,
    transactions: usize,
    mutations: usize,
    bytes: usize,
    highest_version: Option<u64>,
    opened: Option<Instant>,
    /// Where the last closed transaction ended: the position the batch's
    /// checkpoint records.
    file: String,
    pos: u64,
    passed_fences: BTreeSet<usize>,
    /// Targets holding staged pieces of a spilled transaction. Storing the
    /// batch publishes them, and the batch is due as soon as it has any.
    staged: BTreeSet<usize>,
}

impl ApplyBatch {
    const fn is_empty(&self) -> bool {
        self.transactions == 0
    }

    /// Whether the batch has grown or aged enough to be written out. A
    /// commit budget makes every transaction its own batch: the budget
    /// exists so a caller can stop after exactly that many.
    fn is_due(&self, commit_budget: Option<usize>) -> bool {
        !self.is_empty()
            && (commit_budget.is_some()
                || !self.staged.is_empty()
                || self.transactions >= BATCH_TRANSACTIONS
                || self.bytes >= BATCH_BYTES
                || self
                    .opened
                    .is_some_and(|opened| opened.elapsed() >= BATCH_AGE))
    }
}

/// The last position the checkpoint durably holds, and the highest row
/// version stored when it was taken.
#[derive(Default)]
struct DurablePoint {
    file: String,
    pos: u64,
    floor: u64,
}

/// Where one catch-up's time went, for the debug line that ends it.
#[derive(Default)]
struct PhaseTimes {
    batches: usize,
    read: Duration,
    decode: Duration,
    ingest: Duration,
    sync: Duration,
    checkpoint: Duration,
}

impl std::fmt::Display for PhaseTimes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "batches={} read_ms={} decode_ms={} ingest_ms={} sync_ms={} checkpoint_ms={}",
            self.batches,
            self.read.as_millis(),
            self.decode.as_millis(),
            self.ingest.as_millis(),
            self.sync.as_millis(),
            self.checkpoint.as_millis()
        )
    }
}

/// What one flush made durable.
struct FlushedBatch {
    transactions: usize,
    mutations: usize,
    checkpoint: CdcCheckpoint,
    passed_fences: BTreeSet<usize>,
}

/// The version floor a stream starts from.
///
/// The floor is the highest row version the targets hold, and a transaction
/// versioned at or below it means the source's numbering restarted. A
/// process that died between writing a batch and checkpointing it left rows
/// above the checkpoint, though, and the replay of those same transactions
/// then read as a restarted source: every kill in that window cost a full
/// copy of the database. The apply records what it is about to write before
/// it writes; when that record is still there, names this checkpoint, and
/// accounts for everything stored, the floor is the one the checkpoint had.
fn resume_floor(
    metadata: &MetaStore,
    database_id: &str,
    position: &StreamPosition,
    stored: u64,
) -> Result<u64, CdcError> {
    Ok(match metadata.cdc_apply_intent(database_id)? {
        Some(intent)
            if intent.binlog_file == position.file
                && intent.binlog_pos == position.pos
                && stored <= intent.highest =>
        {
            intent.floor.min(stored)
        }
        _ => stored,
    })
}

/// Closes the open transaction into the batch at its commit.
///
/// A transaction that spilled is read back a row at a time. No more than
/// `stage_bytes` of it waits in memory: past that, the table holding the
/// most rows has them written to its store as staged segments, which no
/// reader sees until the batch is stored and every staged table publishes
/// its pieces in one step. Memory therefore stays bounded however large the
/// transaction, and a reader still sees all of it on a table or none.
///
/// The caller stores the batch before sealing a spilled transaction. Staging
/// puts this transaction's rows in segments, and rows of an earlier
/// transaction reaching the same table's memtable afterwards would be read
/// as the newer ones.
fn seal_transaction(
    position: &mut StreamPosition,
    pending: &mut PendingTransaction,
    batch: &mut ApplyBatch,
    targets: &mut [CdcTarget],
    stage_bytes: usize,
) -> Result<(), CdcError> {
    let spilled = pending.spill.is_some();
    if spilled {
        if !batch.is_empty() {
            return Err(CdcError::TransactionSpill(
                "a spilled transaction was sealed into a batch that still held others".to_owned(),
            ));
        }
        // Pieces a failed attempt left behind belong to no transaction.
        for target in targets.iter_mut() {
            target.store.discard_staged();
        }
    }
    let mut waiting: BTreeMap<usize, (usize, Vec<StoredRow>)> = BTreeMap::new();
    let mut waiting_bytes = 0_usize;
    let mut staged = BTreeSet::new();
    let mut highest_version = batch.highest_version;
    let mut mutations = 0_usize;
    let mut bytes = 0_usize;
    pending.for_each_mutation(|mutation| {
        let version = mutation.row.version();
        highest_version = Some(highest_version.map_or(version, |highest| highest.max(version)));
        mutations += 1;
        let row_bytes = mutation.row.estimated_bytes();
        if !spilled {
            bytes = bytes.saturating_add(row_bytes);
            batch
                .rows
                .entry(mutation.target_index)
                .or_default()
                .push(mutation.row);
            return Ok(());
        }
        let held = waiting.entry(mutation.target_index).or_default();
        held.0 = held.0.saturating_add(row_bytes);
        held.1.push(mutation.row);
        waiting_bytes = waiting_bytes.saturating_add(row_bytes);
        if waiting_bytes >= stage_bytes
            && let Some(index) = waiting
                .iter()
                .max_by_key(|(_, (held_bytes, _))| *held_bytes)
                .map(|(index, _)| *index)
            && let Some((held_bytes, rows)) = waiting.remove(&index)
        {
            waiting_bytes = waiting_bytes.saturating_sub(held_bytes);
            stage_rows(targets, index, rows)?;
            staged.insert(index);
        }
        Ok(())
    })?;
    for (index, (held_bytes, rows)) in waiting {
        if staged.contains(&index) {
            stage_rows(targets, index, rows)?;
        } else {
            bytes = bytes.saturating_add(held_bytes);
            batch.rows.entry(index).or_default().extend(rows);
        }
    }
    if !staged.is_empty() {
        recovery_point("cdc.after_stage")?;
    }
    batch.staged.append(&mut staged);
    batch.highest_version = highest_version;
    batch.mutations += mutations;
    batch.bytes = batch.bytes.saturating_add(bytes);
    if let Some(highest) = batch.highest_version {
        position.floor = position.floor.max(highest);
    }
    position.commit_gtid()?;
    batch.file.clone_from(&position.file);
    batch.pos = position.pos;
    batch.transactions += 1;
    batch.opened.get_or_insert_with(Instant::now);
    batch.passed_fences.append(&mut pending.passed_fences);
    *pending = PendingTransaction::default();
    Ok(())
}

fn stage_rows(
    targets: &mut [CdcTarget],
    index: usize,
    rows: Vec<StoredRow>,
) -> Result<(), CdcError> {
    let target = targets
        .get_mut(index)
        .ok_or_else(|| CdcError::TransactionSpill(format!("no target {index} to stage rows in")))?;
    target
        .store
        .stage_cdc(rows)
        .map_err(|error| CdcError::from(error).for_table(&target.source.name))
}

fn publish_staged_tables(
    targets: &mut [CdcTarget],
    staged: &BTreeSet<usize>,
) -> Result<(), CdcError> {
    for index in staged {
        let target = &mut targets[*index];
        target
            .store
            .publish_staged()
            .map_err(|error| CdcError::from(error).for_table(&target.source.name))?;
        recovery_point("cdc.after_staged_publish")?;
    }
    Ok(())
}

/// Runs `apply` over `items`, on several threads when there are several.
/// Reports the failure of the earliest item, so the same input fails the
/// same way however the threads interleave.
fn for_each_table<T: Send>(
    items: Vec<T>,
    parallel: bool,
    apply: impl Fn(T) -> Result<(), CdcError> + Sync,
) -> Result<(), CdcError> {
    if !parallel || items.len() < 2 {
        return items.into_iter().try_for_each(apply);
    }
    let workers = items.len().min(FLUSH_WORKERS);
    let queue = Mutex::new(items.into_iter().enumerate());
    let failure = Mutex::new(None::<(usize, CdcError)>);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().unwrap_or_else(PoisonError::into_inner).next();
                    let Some((order, item)) = next else {
                        break;
                    };
                    if let Err(error) = apply(item) {
                        let mut failure = failure.lock().unwrap_or_else(PoisonError::into_inner);
                        if failure.as_ref().is_none_or(|(first, _)| order < *first) {
                            *failure = Some((order, error));
                        }
                    }
                }
            });
        }
    });
    match failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// Stores every transaction in the batch and checkpoints the position of the
/// last one: each touched table gets its rows as one WAL record, every such
/// WAL is synchronized, and one metadata commit then moves the checkpoint
/// past the whole batch. A crash anywhere before that commit leaves the
/// checkpoint where it was, and the replay writes the same rows at the same
/// versions.
fn flush_batch(
    targets: &mut [CdcTarget],
    metadata: &mut MetaStore,
    database_id: &str,
    position: &StreamPosition,
    batch: &mut ApplyBatch,
    durable: &mut DurablePoint,
    phases: &mut PhaseTimes,
) -> Result<FlushedBatch, CdcError> {
    let started = Instant::now();
    let mut rows = std::mem::take(&mut batch.rows);
    let touched = rows
        .keys()
        .chain(&batch.staged)
        .copied()
        .collect::<BTreeSet<_>>();
    if let Some(highest) = batch.highest_version {
        metadata.record_cdc_apply_intent(
            database_id,
            &CdcApplyIntent {
                floor: durable.floor,
                highest,
                binlog_file: durable.file.clone(),
                binlog_pos: durable.pos,
            },
        )?;
    }
    let work = targets
        .iter_mut()
        .enumerate()
        .filter_map(|(index, target)| rows.remove(&index).map(|rows| (target, rows)))
        .collect::<Vec<_>>();
    for_each_table(
        work,
        batch.mutations >= PARALLEL_INGEST_ROWS,
        |(target, rows)| {
            target
                .store
                .ingest_cdc_in_order(rows)
                .map(drop)
                .map_err(|error| CdcError::from(error).for_table(&target.source.name))?;
            // Some tables hold the batch and the others do not yet.
            recovery_point("cdc.after_table_ingest")
        },
    )?;
    // The pieces of a spilled transaction become visible here, a table at a
    // time and each table's in one step, after the record of what this
    // batch writes and before the checkpoint that covers it.
    publish_staged_tables(targets, &batch.staged)?;
    let ingested = Instant::now();
    phases.ingest += ingested - started;
    recovery_point("cdc.after_ingest")?;
    let synchronize = |target: &mut CdcTarget| {
        target
            .store
            .checkpoint()
            .map_err(|error| CdcError::from(error).for_table(&target.source.name))
    };
    let mut unsynchronized = targets
        .iter_mut()
        .enumerate()
        .filter(|(index, _)| touched.contains(index))
        .map(|(_, target)| target)
        .collect::<Vec<_>>();
    if unsynchronized.len() > 1 {
        synchronize(unsynchronized.remove(0))?;
        recovery_point("cdc.after_first_table_sync")?;
    }
    for_each_table(unsynchronized, true, synchronize)?;
    let synchronized = Instant::now();
    phases.sync += synchronized - ingested;
    let checkpoint = CdcCheckpoint {
        binlog_file: batch.file.clone(),
        binlog_pos: batch.pos,
        ..position.checkpoint()?
    };
    let touched_names = touched
        .iter()
        .map(|index| targets[*index].source.name.clone())
        .collect::<Vec<_>>();
    let checkpoint_record = SnapshotCheckpointRecord {
        kind: checkpoint.kind.clone(),
        gtid_set: checkpoint.gtid_set.clone(),
        binlog_file: Some(checkpoint.binlog_file.clone()),
        binlog_pos: Some(checkpoint.binlog_pos),
    };
    recovery_point("cdc.before_checkpoint_commit")?;
    metadata.commit_cdc_checkpoint(
        database_id,
        &checkpoint_record,
        &touched_names,
        &Utc::now().to_rfc3339(),
    )?;
    recovery_point("cdc.after_checkpoint_commit")?;
    phases.checkpoint += synchronized.elapsed();
    phases.batches += 1;
    durable.file.clone_from(&batch.file);
    durable.pos = batch.pos;
    if let Some(highest) = batch.highest_version {
        durable.floor = durable.floor.max(highest);
    }
    let flushed = FlushedBatch {
        transactions: batch.transactions,
        mutations: batch.mutations,
        checkpoint,
        passed_fences: std::mem::take(&mut batch.passed_fences),
    };
    *batch = ApplyBatch::default();
    Ok(flushed)
}

/// A crash-consistency boundary on the apply path. In a build with
/// failpoints it is the failpoint of the same name; the in-process
/// simulation arms one site at a time to stop the apply there, as a process
/// death at that instant would.
fn recovery_point(site: &'static str) -> Result<(), CdcError> {
    pintail_failpoint::hit(site).map_err(|source| StoreError::Io {
        action: "recovery failpoint".to_owned(),
        source,
    })?;
    #[cfg(test)]
    simulation::crash_if_armed(site)?;
    Ok(())
}

/// Re-checks every paused table this run passed changes over. The run
/// decides what to drop from the paused set it read when it started, so a
/// table resumed part-way through has its later events dropped too, and the
/// write that would have flagged it found the table already running and did
/// nothing. Asking again at the end settles it: a table still paused keeps
/// its flag, and one that resumed under a dropped change is quarantined for
/// the recopy that is the only way to get those rows back.
fn settle_paused_skips(
    metadata: &MetaStore,
    database_id: &str,
    targets: &[CdcTarget],
    paused_skipped: &BTreeSet<usize>,
) -> Result<(), CdcError> {
    for index in paused_skipped {
        if let Some(target) = targets.get(*index) {
            metadata.mark_table_paused_skipped(database_id, &target.source.name)?;
        }
    }
    Ok(())
}

/// Tables whose compaction one finished stream may start. Each merge is
/// bounded by the store's per-pass row budget and runs on a thread of its
/// own, so this is also how many such threads a stream leaves behind it.
const MERGES_STARTED_PER_STREAM: usize = 2;

/// Moves the tables' compaction forward at the end of a stream: a table
/// whose writes stopped has no flush left to do it. Starts at a different
/// table each time so the same few are not always first.
fn maintain_targets(targets: &mut [CdcTarget], stop: Option<&CycleStop>) {
    static ROTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let count = targets.len();
    if count == 0 {
        return;
    }
    let first = ROTATION.fetch_add(1, Ordering::Relaxed) % count;
    let mut running = 0;
    for offset in 0..count {
        if running >= MERGES_STARTED_PER_STREAM || stop.is_some_and(CycleStop::requested) {
            return;
        }
        let target = &mut targets[(first + offset) % count];
        match target.store.maintain() {
            Ok(true) => running += 1,
            Ok(false) => {}
            // Unmerged segments still answer correctly; the next stream
            // tries again.
            Err(error) => pintail_log::log_debug!(
                "cdc maintenance of {} deferred: {error}",
                target.source.name
            ),
        }
    }
}

fn finish_result(
    commits: usize,
    mutations: usize,
    position: &StreamPosition,
    mut targets: Vec<CdcTarget>,
    stop: Option<&CycleStop>,
) -> Result<CdcResult, CdcError> {
    maintain_targets(&mut targets, stop);
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
    /// The highest row version the targets hold. The newest version wins,
    /// so a transaction versioned at or below it would be applied and then
    /// lose to the very row it replaced.
    floor: u64,
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
                floor: 0,
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
                floor: 0,
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
        let mut request = match self.requested_file() {
            Some((file, pos)) => BinlogStreamRequest::new(server_id)
                .with_filename(file.as_bytes())
                .with_pos(pos),
            None => BinlogStreamRequest::new(server_id)
                .with_pos(4)
                .with_gtid()
                .with_gtid_set(self.gtid_set.as_ref().expect("GTID set").to_sids()?),
        };
        if !blocking {
            request = request.with_non_blocking();
        }
        Ok(request)
    }

    /// The file and offset a resume names, or `None` when the GTID set alone
    /// says where. A file name beside the set makes the source look that
    /// file up before it reads the set, so a source whose binlogs were
    /// renumbered - an upgrade, a restore - refused with 1236 and forced a
    /// full copy it did not need.
    fn requested_file(&self) -> Option<(&str, u64)> {
        match self.kind {
            PositionKind::MysqlGtid => None,
            PositionKind::FilePosition => Some((self.file.as_str(), self.pos)),
        }
    }

    /// How many row mutations one source transaction may carry: 24 ordinal
    /// bits under GTID, 16 under file-position, matching the version layout.
    fn ordinal_budget(&self) -> u32 {
        match self.kind {
            PositionKind::MysqlGtid => 0xFF_FFFF,
            PositionKind::FilePosition => 0xFFFF,
        }
    }

    fn version(&self, event_position: u64, ordinal: u32) -> Result<u64, CdcError> {
        if let Some(gtid) = &self.pending_gtid {
            // 24 ordinal bits, not 16. A production backfill routinely
            // commits hundreds of thousands of rows in one transaction, and
            // the old 65,535-mutation budget quarantined the table the first
            // time one arrived - measured by the browser soak at its very
            // first 256k-row batch. Growing the ordinal is upgrade-safe
            // because GTID sequences only increase: for any seq2 > seq1,
            // seq2 << 24 exceeds seq1 << 16, so every new version stays
            // above every stored one; and a transaction applies atomically
            // at commit, so no single transaction ever spans encodings.
            // 40 bits of sequence remain - a trillion transactions.
            let ordinal = u64::from(ordinal) + 1;
            if ordinal > 0xFF_FFFF {
                return Err(CdcError::Decode(
                    "one source transaction exceeds 16,777,215 row mutations".to_owned(),
                ));
            }
            return gtid
                .sequence
                .checked_shl(24)
                .and_then(|base| base.checked_add(ordinal))
                .ok_or_else(|| CdcError::Decode("GTID version exceeds UInt64".to_owned()));
        }
        // File-position mode has no spare bits: 16 for the file index, 32
        // for the byte position, 16 for the ordinal. The budget stays at
        // 65,535 mutations per transaction there - recorded in
        // docs/limitations.md; GTID mode is the fix.
        let ordinal = u16::try_from(ordinal + 1).map_err(|_| {
            CdcError::Decode(
                "one source transaction exceeds 65,535 row mutations                  (file-position mode; GTID mode raises the budget to 16,777,215)"
                    .to_owned(),
            )
        })?;
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

    /// Why the transaction just opened cannot be applied in order, if it
    /// cannot. A source rebuilt under a new server identity numbers its
    /// transactions from one again; resumed by GTID, each change it sent
    /// would version below the stored row it replaces and lose to it,
    /// silently. Only a fresh copy puts the two back in one order.
    fn out_of_order(&self, event_position: u64) -> Result<Option<String>, CdcError> {
        let first = self.version(event_position, 0)?;
        Ok((first <= self.floor).then(|| {
            let transaction = self.pending_gtid.as_ref().map_or_else(
                || format!("{}:{event_position}", self.file),
                |gtid| format!("GTID sequence {}", gtid.sequence),
            );
            format!(
                "source transaction {transaction} versions at {first}, at or below the \
                 {floor} already stored; the source's transaction numbering restarted",
                floor = self.floor
            )
        }))
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

/// Whether this database's start line says anything its last one did not.
///
/// A process that has just started has no previous line for any database, so
/// the first pass after a restart always reports - which is the pass an
/// operator most wants to see.
fn start_summary_changed(database_id: &str, summary: &str) -> bool {
    static SUMMARIES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, String>>,
    > = std::sync::OnceLock::new();
    let summaries =
        SUMMARIES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    // A poisoned lock means a thread panicked mid-update; reporting the line
    // is the safe direction, because the alternative is a silent replication
    // log.
    let Ok(mut summaries) = summaries.lock() else {
        return true;
    };
    if summaries
        .get(database_id)
        .is_some_and(|last| last == summary)
    {
        return false;
    }
    summaries.insert(database_id.to_owned(), summary.to_owned());
    true
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

    #[test]
    fn an_unreadable_create_still_names_its_table() {
        let name = |sql: &str| super::created_table_name(sql, "app");
        assert_eq!(
            name("CREATE TABLE t2 (\n  fld1 int(6) unsigned zerofill) charset utf8mb4"),
            Some("t2".to_owned())
        );
        assert_eq!(
            name("create table if not exists `odd``name`(a int)"),
            Some("odd`name".to_owned())
        );
        assert_eq!(name("CREATE TABLE app.t3 LIKE t2"), Some("t3".to_owned()));
        assert_eq!(
            name("CREATE TABLE `app`.`dotted.name` (a int)"),
            Some("dotted.name".to_owned())
        );
        assert_eq!(
            name("CREATE TABLE other.t4 (a int)"),
            None,
            "another schema"
        );
        assert_eq!(
            name("CREATE TEMPORARY TABLE t5 (a int)"),
            None,
            "never in row events"
        );
        assert_eq!(name("ALTER TABLE t2 ADD COLUMN b int"), None);
        assert_eq!(name("CREATE INDEX i ON t2 (a)"), None);
    }

    /// Which tables an unreadable DDL quarantines. Saying yes too often
    /// costs a resync; saying no too rarely leaves the replica disagreeing
    /// with its source, so the boundary check errs toward yes.
    #[test]
    fn an_unreadable_statement_names_the_tables_it_mentions() {
        use super::statement_names_table;
        let create = "CREATE TABLE \"CertificateTemplate\" (\"id\" INT)";
        assert!(statement_names_table(create, "CertificateTemplate"));
        assert!(statement_names_table(create, "certificatetemplate"));
        // A different table of similar spelling is not named.
        assert!(!statement_names_table(create, "Certificate"));
        assert!(!statement_names_table(create, "TemplateVersion"));
        // Backticks, qualifiers and trailing punctuation still delimit it.
        assert!(statement_names_table(
            "ALTER TABLE `app`.`events` ADD COLUMN x INT",
            "events"
        ));
        assert!(!statement_names_table(
            "ALTER TABLE `app`.`events2` ADD x INT",
            "events"
        ));
        assert!(!statement_names_table("CREATE TABLE t (id INT)", ""));
    }
    use super::{
        CdcOptions, CdcTarget, PendingMutation, PendingTransaction, StreamPosition,
        generated_server_id, new_table_matches, push_mutations, sanitize_binlog_filename,
    };
    use pintail_meta::{MetaStore, SnapshotCheckpointRecord};
    use pintail_probe::{SourceColumn, SourceFlavor, SourceKey, SourceTable};
    use pintail_store::{StoreOptions, TableStore};
    use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};

    /// A source that can stream, with no tables of its own.
    fn streamable_source() -> pintail_probe::ProbeReport {
        serde_json::from_str(
            r#"{
                "database": "app",
                "server": {
                    "version": "8.4.0",
                    "version_comment": "MySQL Community Server",
                    "flavor": "mysql"
                },
                "variables": {},
                "grants": [],
                "capabilities": {
                    "log_bin": true,
                    "row_binlog": true,
                    "full_row_image": true,
                    "full_row_metadata": true,
                    "replication_grants": true,
                    "global_read_lock": true,
                    "gtid_available": true,
                    "recommended_mode": "cdc",
                    "reasons": []
                },
                "tables": [],
                "warnings": []
            }"#,
        )
        .expect("probe report")
    }

    /// Every tracked table dropped and re-created leaves nothing to open:
    /// dropped tables are retained for reading, not streamed. Refusing that
    /// run stranded the database, because the CREATE events that supersede
    /// the retained rows are only ever read by a run.
    #[test]
    fn a_run_with_no_targets_is_allowed_when_it_can_adopt_new_tables() {
        use super::validate_configuration;
        let report = streamable_source();
        let adopting = CdcOptions {
            auto_include_new_tables: true,
            new_table_root: Some(std::path::PathBuf::from("/tables")),
            ..CdcOptions::default()
        };
        validate_configuration(&report, &[], &adopting).expect("a run that can adopt is allowed");

        // Without a root there is nowhere to put an adopted table, so an
        // empty run really has nothing to do and stays a configuration error.
        let barren = CdcOptions {
            auto_include_new_tables: true,
            new_table_root: None,
            ..CdcOptions::default()
        };
        assert!(validate_configuration(&report, &[], &barren).is_err());
        let excluded = CdcOptions {
            auto_include_new_tables: false,
            new_table_root: Some(std::path::PathBuf::from("/tables")),
            ..CdcOptions::default()
        };
        assert!(validate_configuration(&report, &[], &excluded).is_err());
    }

    /// Superseding a generation removes the old files first and commits the
    /// metadata last. Committed first, a crash in between leaves a live
    /// table row beside the old generation's files, and every later cycle
    /// is refused when it opens them - change capture stops for the whole
    /// database. Proven here by failing the removal: the row must still be
    /// marked dropped.
    #[cfg(unix)]
    #[test]
    fn a_supersession_that_cannot_remove_the_old_files_commits_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let workspace = tempfile::tempdir().expect("tempdir");
        let root = workspace.path().join("tables");
        let old = super::new_table_directory(&root, "events");
        std::fs::create_dir_all(&old).expect("old generation");
        std::fs::write(old.join("table.wal"), b"old").expect("old file");
        std::fs::create_dir_all(root.join(".probe")).expect("probe");

        let metadata_path = workspace.path().join("meta.db");
        let metadata = MetaStore::open(&metadata_path).expect("metadata");
        metadata
            .upsert_database("source", "app", b"unused", "2026-09-14T00:00:00Z")
            .expect("database");
        metadata
            .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
            .expect("table");
        metadata
            .mark_table_orphaned(
                "source",
                "events",
                "DROP TABLE events",
                "2026-09-14T00:00:01Z",
            )
            .expect("orphan");

        let mut permissions = std::fs::metadata(&root).expect("root").permissions();
        let original = permissions.mode();
        permissions.set_mode(0o555);
        std::fs::set_permissions(&root, permissions).expect("seal the root");
        // A privileged runner ignores the seal, and then this proves nothing.
        let sealed = std::fs::remove_dir_all(root.join(".probe")).is_err();

        let outcome = super::supersede_generation(
            &metadata,
            "source",
            &root,
            &mut [],
            &std::collections::BTreeMap::new(),
            "events",
            true,
        );

        let mut restored = std::fs::metadata(&root).expect("root").permissions();
        restored.set_mode(original);
        std::fs::set_permissions(&root, restored).expect("unseal the root");
        if !sealed {
            return;
        }

        assert!(outcome.is_err(), "the removal must fail under the seal");
        let records = metadata.tables("source").expect("tables");
        assert!(
            records[0].orphaned_at.is_some(),
            "the table stays dropped until its old files are gone"
        );
    }

    /// A process killed between writing a batch and checkpointing it leaves
    /// rows above the checkpoint. Their replay is in order; only rows nobody
    /// announced, or a checkpoint that has moved since, keep the stored floor.
    #[test]
    fn rows_an_unfinished_apply_left_past_the_checkpoint_do_not_raise_the_floor() {
        let workspace = tempfile::tempdir().expect("CDC workspace");
        let mut metadata =
            MetaStore::open(&workspace.path().join("pintail-meta.db")).expect("metadata");
        metadata
            .upsert_database("source", "app", b"unused", "2026-09-24T00:00:00Z")
            .expect("database");
        let checkpoint = |pos| SnapshotCheckpointRecord {
            kind: "filepos".to_owned(),
            gtid_set: None,
            binlog_file: Some("mysql-bin.000003".to_owned()),
            binlog_pos: Some(pos),
        };
        let at = |pos| {
            StreamPosition::from_checkpoint(checkpoint(pos), SourceFlavor::Mysql).expect("position")
        };
        let floor = |metadata: &MetaStore, pos, stored| {
            crate::resume_floor(metadata, "source", &at(pos), stored).expect("floor")
        };
        assert_eq!(floor(&metadata, 900, 500), 500, "nothing was announced");

        metadata
            .record_cdc_apply_intent(
                "source",
                &pintail_meta::CdcApplyIntent {
                    floor: 200,
                    highest: 500,
                    binlog_file: "mysql-bin.000003".to_owned(),
                    binlog_pos: 900,
                },
            )
            .expect("intent");
        assert_eq!(
            floor(&metadata, 900, 500),
            200,
            "the whole batch was stored"
        );
        assert_eq!(
            floor(&metadata, 900, 350),
            200,
            "part of the batch was stored"
        );
        assert_eq!(
            floor(&metadata, 900, 150),
            150,
            "none of the batch was stored"
        );
        assert_eq!(floor(&metadata, 900, 501), 501, "rows nobody announced");
        assert_eq!(floor(&metadata, 901, 500), 500, "another checkpoint");

        metadata
            .commit_cdc_checkpoint("source", &checkpoint(950), &[], "2026-09-24T00:00:01Z")
            .expect("checkpoint");
        assert_eq!(metadata.cdc_apply_intent("source").expect("intent"), None);
        assert_eq!(floor(&metadata, 950, 500), 500, "the apply finished");
    }

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
    fn a_gtid_resume_names_no_file_so_renumbered_binlogs_still_resume() {
        let checkpoint = |kind: &str| SnapshotCheckpointRecord {
            kind: kind.to_owned(),
            gtid_set: Some("3E11FA47-71CA-11E1-9E33-C80AA9429562:1-4".to_owned()),
            binlog_file: Some("binlog.000462".to_owned()),
            binlog_pos: Some(30_508),
        };
        let gtid = StreamPosition::from_checkpoint(checkpoint("gtid"), SourceFlavor::Mysql)
            .expect("gtid position");
        assert_eq!(gtid.requested_file(), None);
        gtid.request(7, true).expect("a GTID request encodes");
        let filepos = StreamPosition::from_checkpoint(checkpoint("filepos"), SourceFlavor::Mysql)
            .expect("file position");
        assert_eq!(filepos.requested_file(), Some(("binlog.000462", 30_508)));
    }

    #[test]
    fn a_transaction_numbered_below_the_stored_rows_is_refused() {
        let mut position = StreamPosition::from_checkpoint(
            SnapshotCheckpointRecord {
                kind: "gtid".to_owned(),
                gtid_set: Some("3E11FA47-71CA-11E1-9E33-C80AA9429562:1-900".to_owned()),
                binlog_file: Some("binlog.000462".to_owned()),
                binlog_pos: Some(4),
            },
            SourceFlavor::Mysql,
        )
        .expect("position");
        let open = |position: &mut StreamPosition, sequence| {
            position.pending_gtid = Some(super::GtidIdentity {
                sid: [7; 16],
                tag: None,
                sequence,
            });
        };
        // Freshly copied rows sit at version zero: any transaction follows.
        open(&mut position, 1);
        assert_eq!(position.out_of_order(120).expect("check"), None);

        // Rows streamed up to sequence 900; the source was rebuilt under a
        // new identity and numbers from one again.
        open(&mut position, 900);
        position.floor = position.version(120, 3).expect("stored");
        open(&mut position, 901);
        assert_eq!(position.out_of_order(120).expect("check"), None);
        open(&mut position, 2);
        let reason = position
            .out_of_order(120)
            .expect("check")
            .expect("a restarted numbering is refused");
        assert!(reason.contains("GTID sequence 2"), "{reason}");
    }

    #[test]
    fn gtid_versions_carry_backfill_sized_transactions() {
        let mut position = StreamPosition::from_checkpoint(
            SnapshotCheckpointRecord {
                kind: "gtid".to_owned(),
                gtid_set: Some("3E11FA47-71CA-11E1-9E33-C80AA9429562:1-4".to_owned()),
                binlog_file: Some("mysql-bin.000002".to_owned()),
                binlog_pos: Some(4),
            },
            SourceFlavor::Mysql,
        )
        .expect("position");
        position.pending_gtid = Some(super::GtidIdentity {
            sid: [7; 16],
            tag: None,
            sequence: 5,
        });
        // The soak's first 256k-row backfill batch quarantined its table
        // under the old 65,535-mutation budget; that size must encode.
        position
            .version(200, 256_000)
            .expect("a 256k-row transaction encodes");
        assert!(
            position.version(200, 100_000).expect("low")
                < position.version(200, 100_001).expect("high")
        );
        // The 24-bit budget still refuses the truly absurd, by name.
        let refusal = position.version(200, 0xFF_FFFF).expect_err("over budget");
        assert!(refusal.to_string().contains("16,777,215"));

        // Upgrade safety: any later transaction's version under the 24-bit
        // layout exceeds any earlier one stored under the old 16-bit layout,
        // because GTID sequences only increase.
        let old_layout_ceiling = (5_u64 << 16) | 0xFFFF;
        position.pending_gtid = Some(super::GtidIdentity {
            sid: [7; 16],
            tag: None,
            sequence: 6,
        });
        assert!(position.version(200, 0).expect("next transaction") > old_layout_ceiling);
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
        for (previous, refreshed) in [(&keyless, primary.clone()), (&primary, keyless.clone())] {
            let refusal = pintail_probe::stabilize_source_table(previous, refreshed)
                .expect_err("a key change is a resnapshot boundary");
            // The marker is what the store-rebuilding path matches on, so the
            // reason has to carry it and not just read well.
            assert!(
                refusal.starts_with(pintail_probe::IN_PLACE_REFUSAL)
                    && refusal.contains("physical key"),
                "{refusal}",
            );
        }
    }

    #[test]
    fn tracked_reopen_restores_collation_from_schema_history() {
        let workspace = tempfile::tempdir().expect("CDC workspace");
        let metadata_path = workspace.path().join("pintail-meta.db");
        let table_directory = workspace.path().join("events");
        let mut current = source_table(KeyMode::Primary);
        current.columns.push(SourceColumn {
            id: 2,
            name: "label".to_owned(),
            mysql_data_type: "varchar".to_owned(),
            mysql_column_type: "varchar(64)".to_owned(),
            pintail_type: DataType::Utf8,
            nullable: true,
            character_set: Some("utf8mb4".to_owned()),
            collation: Some("utf8mb4_0900_ai_ci".to_owned()),
            generated_stored: false,
            generation_expression: String::new(),
            generation_captured: true,
            extra: String::new(),
            auto_increment: false,
            default_value: None,
            default_generated: false,
            ordinal: 0,
        });

        let store = TableStore::open(
            &table_directory,
            current
                .table_schema_with_version(2)
                .expect("current table schema"),
            StoreOptions::default(),
        )
        .expect("table store");
        drop(store);

        let mut metadata = MetaStore::open(&metadata_path).expect("metadata");
        metadata
            .upsert_database("source", "app", b"unused", "2026-08-08T00:00:00Z")
            .expect("database");
        metadata
            .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
            .expect("table");
        metadata
            .record_schema_history(
                "source",
                "events",
                2,
                Some("ALTER TABLE events ADD COLUMN label VARCHAR(64)"),
                &serde_json::to_string(&current.columns).expect("columns JSON"),
                "2026-08-08T00:00:01Z",
            )
            .expect("schema history");
        drop(metadata);

        let stale_probe = source_table(KeyMode::Primary);
        let reopened = CdcTarget::open_tracked(
            &metadata_path,
            "source",
            stale_probe,
            &table_directory,
            StoreOptions::default(),
        )
        .expect("tracked reopen");
        let label = &reopened.source().columns[1];
        assert_eq!(label.character_set.as_deref(), Some("utf8mb4"));
        assert_eq!(label.collation.as_deref(), Some("utf8mb4_0900_ai_ci"));
        assert_eq!(
            reopened.store().schema().columns()[1].collation(),
            Some("utf8mb4_0900_ai_ci")
        );
    }

    /// An unaltered table's shape must not follow the stored probe once its
    /// store exists: a re-probe records the source's current columns, and an
    /// ALTER still waiting in the binlog then redefined version 1 under a
    /// store holding the old ones, which refused to open with a fingerprint
    /// mismatch - every cycle, until someone resynced the table.
    #[test]
    fn a_reprobe_ahead_of_the_stream_does_not_redefine_the_first_generation() {
        let workspace = tempfile::tempdir().expect("CDC workspace");
        let metadata_path = workspace.path().join("pintail-meta.db");
        let table_directory = workspace.path().join("events");
        let metadata = MetaStore::open(&metadata_path).expect("metadata");
        metadata
            .upsert_database("source", "app", b"unused", "2026-09-24T00:00:00Z")
            .expect("database");
        metadata
            .upsert_snapshot_table("source", "events", Some("[\"id\"]"), Some("[\"id\"]"))
            .expect("table");
        drop(metadata);

        let copied = source_table(KeyMode::Primary);
        let first = CdcTarget::open_tracked(
            &metadata_path,
            "source",
            copied.clone(),
            &table_directory,
            StoreOptions::default(),
        )
        .expect("first open");
        let mut store = first.into_store();
        store
            .ingest(vec![StoredRow::new(
                PrimaryKey::new(vec![KeyPart::Int64(1)]).expect("key"),
                vec![Value::Int64(1)],
                1,
                false,
            )])
            .expect("ingest");
        store.flush().expect("flush");
        drop(store);

        // The source gained a column; the probe saw it before the stream did.
        let mut reprobed = copied.clone();
        let mut added = reprobed.columns[0].clone();
        added.id = reprobed
            .columns
            .iter()
            .map(|column| column.id)
            .max()
            .unwrap_or(0)
            + 1;
        added.name = "added".to_owned();
        added.nullable = true;
        reprobed.columns.push(added);
        let reopened = CdcTarget::open_tracked(
            &metadata_path,
            "source",
            reprobed,
            &table_directory,
            StoreOptions::default(),
        )
        .expect("the store opens at the shape it was written with");
        assert_eq!(reopened.source().columns.len(), copied.columns.len());
        let history = MetaStore::open(&metadata_path)
            .expect("metadata")
            .schema_history("source", "events")
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].version, 1);
    }

    fn source_table(mode: KeyMode) -> SourceTable {
        SourceTable {
            name: "events".to_owned(),
            engine: Some("InnoDB".to_owned()),
            estimated_rows: Some(2),
            rows_are_exact: false,
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
                generation_expression: String::new(),
                generation_captured: true,
                extra: String::new(),
                auto_increment: false,
                default_value: None,
                default_generated: false,
                ordinal: 0,
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
            secondary_indexes: Vec::new(),
            warnings: Vec::new(),
            source_column_count: 0,
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
            0xFFFF,
        )
        .expect("spill mutation");

        assert!(pending.spill.is_some());
        assert!(pending.mutations.is_empty());
        let mut mutations = Vec::new();
        pending
            .for_each_mutation(|mutation| {
                mutations.push(mutation);
                Ok(())
            })
            .expect("read spill");
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].target_index, 2);
        assert_eq!(mutations[0].row, row);
    }
}
