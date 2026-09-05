//! Polling and primary-key reconciliation for Pintail.

mod checksum;
mod cursor;
mod decoder;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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
    decoder::{
        decode_key, decode_row, key_part, key_projection, physical_key, quote_identifier,
        source_projection,
    },
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

/// Which targets a CDC reconciliation repairs, and how.
#[derive(Clone, Copy, Debug)]
pub enum CdcReconcileScope<'a> {
    /// Every target, compared row by row against the source.
    Full,
    /// The named targets, repaired through their cascading foreign keys:
    /// a child row whose parent the replica no longer holds is the only
    /// kind of row an invisible cascade can have touched, so those are the
    /// rows verified against the source. The remaining targets serve as
    /// parents. A named table whose cascading keys do not all reference a
    /// replicated parent's primary key is compared in full instead.
    Cascade(&'a [String]),
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
        let outcome = poll_table(
            &mut connection,
            &mut metadata,
            database_id,
            &report.database,
            target,
            &options,
        )
        .await?;
        // A poll cycle runs every poll interval on every table, so an
        // unconditional info line would be the loudest thing in the log while
        // saying nothing. A table that actually moved rows is worth an info
        // line; an idle one is debug. That way a quiet system stays quiet and
        // real work is still visible without switching levels.
        let moved = outcome.ingested > 0 || outcome.tombstones > 0 || outcome.unique_repairs > 0;
        let line = format!(
            "poll db={database_id} table={} strategy={:?} changed={} ingested={} tombstones={} chunks={}/{} repairs={} version={}",
            outcome.table,
            outcome.strategy,
            outcome.changed,
            outcome.ingested,
            outcome.tombstones,
            outcome.chunks_redumped,
            outcome.chunks_scanned,
            outcome.unique_repairs,
            outcome.version
        );
        if moved {
            pintail_log::log_info!("{line}");
        } else {
            pintail_log::log_debug!("{line}");
        }
        outcomes.push(outcome);
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
    scope: CdcReconcileScope<'_>,
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
    let selected = match scope {
        CdcReconcileScope::Full => targets
            .iter()
            .map(|target| target.source.name.clone())
            .collect::<Vec<_>>(),
        CdcReconcileScope::Cascade(names) => cascade_order(names, &targets),
    };
    let mut outcomes = Vec::with_capacity(selected.len());
    for name in selected {
        let index = targets
            .iter()
            .position(|target| target.source.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| {
                PollError::InvalidConfiguration(format!(
                    "reconciliation target {name} is not among the opened targets"
                ))
            })?;
        if targets[index].source.key.mode == KeyMode::AppendRowId {
            return Err(PollError::InvalidConfiguration(format!(
                "{name} has no source key for CDC reconciliation"
            )));
        }
        if !report
            .tables
            .iter()
            .any(|source| source.name.eq_ignore_ascii_case(&name))
        {
            return Err(PollError::InvalidConfiguration(format!(
                "target {name} is absent from the probe report"
            )));
        }
        let parents = match scope {
            CdcReconcileScope::Full => None,
            CdcReconcileScope::Cascade(_) => cascade_parents(&targets, index),
        };
        let target = &mut targets[index];
        let outcome = match parents {
            Some(parents) => {
                reconcile_cascade_target(
                    &mut connection,
                    &mut metadata,
                    database_id,
                    &report.database,
                    target,
                    &parents,
                )
                .await?
            }
            None => {
                reconcile_cdc_target(
                    &mut connection,
                    &mut metadata,
                    database_id,
                    &report.database,
                    target,
                    chunk_rows,
                )
                .await?
            }
        };
        outcomes.push(outcome);
    }
    Ok(CdcReconcileResult {
        tables: outcomes,
        targets,
    })
}

/// A replicated parent a child's cascading foreign key points at, resolved
/// to the parent's primary key so a child row's reference is one point
/// lookup in the parent replica.
struct CascadeParent {
    /// Referencing column names on the child, in the parent's key order.
    columns: Vec<String>,
    /// The parent replica.
    snapshot: pintail_store::TableSnapshot,
}

/// Whether an `ON DELETE`/`ON UPDATE` rule changes child rows without a
/// binlog row event, so `MySQL` never shows the change to a CDC reader.
fn invisible_fk_rule(rule: &str) -> bool {
    rule.eq_ignore_ascii_case("CASCADE") || rule.eq_ignore_ascii_case("SET NULL")
}

/// The parents for a targeted pass over `targets[child]`, or `None` when
/// any cascading key of the child references something a point lookup
/// cannot answer: an unreplicated table, a parent without a primary key,
/// or a unique key that is not the parent's primary key.
fn cascade_parents(targets: &[PollTarget], child: usize) -> Option<Vec<CascadeParent>> {
    let source = &targets[child].source;
    let mut parents = Vec::new();
    for key in source
        .foreign_keys
        .iter()
        .filter(|key| invisible_fk_rule(&key.delete_rule) || invisible_fk_rule(&key.update_rule))
    {
        let parent = targets.iter().find(|target| {
            target
                .source
                .name
                .eq_ignore_ascii_case(&key.referenced_table)
        })?;
        if parent.source.key.mode == KeyMode::AppendRowId
            || parent.source.key.columns.len() != key.referenced_columns.len()
        {
            return None;
        }
        // The child's referencing columns, arranged in the parent's key
        // order, so the parent key is built straight from a child row.
        let mut columns = Vec::with_capacity(key.columns.len());
        for parent_column in &parent.source.key.columns {
            let position = key
                .referenced_columns
                .iter()
                .position(|referenced| referenced.eq_ignore_ascii_case(parent_column))?;
            columns.push(key.columns[position].clone());
        }
        parents.push(CascadeParent {
            columns,
            snapshot: parent.store.snapshot(),
        });
    }
    (!parents.is_empty()).then_some(parents)
}

/// Orders the named tables so a parent is repaired before the children
/// that reference it: a cascade removes rows at every level, and the child
/// pass detects a missing parent, so the parent's tombstones must land
/// first. A cycle keeps the remaining names in their given order.
fn cascade_order(names: &[String], targets: &[PollTarget]) -> Vec<String> {
    let mut remaining = names.to_vec();
    let mut ordered = Vec::with_capacity(names.len());
    while !remaining.is_empty() {
        let independent = remaining.iter().position(|name| {
            targets
                .iter()
                .find(|target| target.source.name.eq_ignore_ascii_case(name))
                .is_none_or(|target| {
                    !target.source.foreign_keys.iter().any(|key| {
                        !key.referenced_table.eq_ignore_ascii_case(name)
                            && remaining
                                .iter()
                                .any(|other| other.eq_ignore_ascii_case(&key.referenced_table))
                    })
                })
        });
        let next = independent.unwrap_or(0);
        ordered.push(remaining.remove(next));
    }
    ordered
}

/// Keys a membership query verifies at once: five thousand keys keep the
/// placeholder count under the prepared-statement limit for keys of up to
/// a dozen columns while amortizing the round trip, which dominated a pass
/// over two million candidates at a thousand keys a query.
const MEMBERSHIP_BATCH: usize = 5_000;

/// The `WHERE` clause and parameters selecting exactly `keys` from a source
/// table, as a single-column `IN` list or a row-constructor `IN` list.
fn key_membership_condition(table: &SourceTable, keys: &[PrimaryKey]) -> (String, Vec<MysqlValue>) {
    let columns = table
        .key
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>();
    let row = if columns.len() == 1 {
        "?".to_owned()
    } else {
        format!("({})", vec!["?"; columns.len()].join(","))
    };
    let list = vec![row.as_str(); keys.len()].join(",");
    let lhs = if columns.len() == 1 {
        columns[0].clone()
    } else {
        format!("({})", columns.join(","))
    };
    (
        format!(" WHERE {lhs} IN ({list})"),
        keys.iter()
            .flat_map(|key| key.parts().iter().map(key_part_mysql_value))
            .collect(),
    )
}

/// Store column ids for `names`, in that order, resolved case-insensitively
/// against the replica schema.
fn column_ids_named(
    snapshot: &pintail_store::TableSnapshot,
    table: &str,
    names: &[String],
) -> Result<Vec<u32>, PollError> {
    names
        .iter()
        .map(|name| {
            snapshot
                .schema()
                .columns()
                .iter()
                .find(|column| column.name().eq_ignore_ascii_case(name))
                .map(pintail_types::Column::id)
                .ok_or_else(|| {
                    PollError::Decode(format!("{table} column {name} is absent from its replica"))
                })
        })
        .collect()
}

/// A key from the values at `positions`, or `None` when any is NULL.
fn key_at(values: &[Value], positions: &[usize]) -> Option<PrimaryKey> {
    let parts = positions
        .iter()
        .map(|&position| key_part(&values[position]))
        .collect::<Option<Vec<_>>>()?;
    PrimaryKey::new(parts).ok()
}

/// The values a tombstone carries: the key's own parts in the key columns,
/// NULL where the column allows it, and the type's zero elsewhere. A
/// tombstone's values are never read back - the row is gone - but the
/// store validates arity and nullability, and reading the replica's real
/// values for every candidate decoded the whole table a second time.
fn tombstone_values(
    schema: &pintail_types::TableSchema,
    source: &SourceTable,
    key: &PrimaryKey,
) -> Vec<Value> {
    schema
        .columns()
        .iter()
        .map(|column| {
            let key_part = source
                .key
                .columns
                .iter()
                .position(|name| name.eq_ignore_ascii_case(column.name()))
                .and_then(|position| key.parts().get(position));
            match key_part {
                Some(pintail_types::KeyPart::Int64(value)) => Value::Int64(*value),
                Some(pintail_types::KeyPart::UInt64(value)) => Value::UInt64(*value),
                Some(pintail_types::KeyPart::Utf8(value)) => Value::Utf8(value.clone()),
                Some(pintail_types::KeyPart::Binary(value)) => Value::Binary(value.clone()),
                None if column.is_nullable() => Value::Null,
                None => match column.data_type().storage_type() {
                    pintail_types::DataType::Boolean => Value::Boolean(false),
                    pintail_types::DataType::Int64 => Value::Int64(0),
                    pintail_types::DataType::UInt64 => Value::UInt64(0),
                    pintail_types::DataType::Float64 => {
                        Value::Float64(pintail_types::Float64::new(0.0))
                    }
                    pintail_types::DataType::Binary => Value::Binary(Vec::new()),
                    _ => Value::Utf8(String::new()),
                },
            }
        })
        .collect()
}

/// Verifies `candidates` against the source: a candidate the source still
/// holds is refreshed with the source's row, one it no longer holds is
/// tombstoned. Returns (refreshed, tombstoned) after ingesting both.
async fn repair_candidates(
    connection: &mut Conn,
    source_database: &str,
    target: &mut PollTarget,
    snapshot: &pintail_store::TableSnapshot,
    candidates: &[PrimaryKey],
    version: u64,
) -> Result<(usize, usize), PollError> {
    let mut refreshed = 0;
    let mut tombstoned = 0;
    for batch in candidates.chunks(MEMBERSHIP_BATCH) {
        let (condition, parameters) = key_membership_condition(&target.source, batch);
        let rows = fetch_rows_page(
            connection,
            source_database,
            &target.source,
            &condition,
            parameters,
            "",
            batch.len(),
            0,
        )
        .await?;
        let mut present = BTreeMap::new();
        for row in rows {
            let decoded = decode_row(&target.source, row, 0, version, false)?;
            present.insert(decoded.key().clone(), decoded);
        }
        let mut repairs = Vec::with_capacity(batch.len());
        for key in batch {
            if let Some(decoded) = present.remove(key) {
                refreshed += 1;
                repairs.push(decoded);
            } else {
                tombstoned += 1;
                repairs.push(StoredRow::new(
                    key.clone(),
                    tombstone_values(snapshot.schema(), &target.source, key),
                    version,
                    true,
                ));
            }
        }
        if !repairs.is_empty() {
            target.store.ingest(repairs)?;
        }
    }
    Ok((refreshed, tombstoned))
}

/// The one projection a targeted pass streams: the child's key, then every
/// referencing column once, with the positions each key is built from.
struct CascadeProjection {
    names: Vec<String>,
    key_positions: Vec<usize>,
    parent_positions: Vec<Vec<usize>>,
}

impl CascadeProjection {
    fn new(source: &SourceTable, parents: &[CascadeParent]) -> Self {
        let mut names = source.key.columns.clone();
        for parent in parents {
            for column in &parent.columns {
                if !names.iter().any(|name| name.eq_ignore_ascii_case(column)) {
                    names.push(column.clone());
                }
            }
        }
        let position_of = |column: &str| {
            names
                .iter()
                .position(|name| name.eq_ignore_ascii_case(column))
                .expect("projected column")
        };
        let key_positions = source
            .key
            .columns
            .iter()
            .map(|column| position_of(column))
            .collect();
        let parent_positions = parents
            .iter()
            .map(|parent| {
                parent
                    .columns
                    .iter()
                    .map(|column| position_of(column))
                    .collect()
            })
            .collect();
        Self {
            names,
            key_positions,
            parent_positions,
        }
    }
}

/// Repairs a cascade-affected child through its parents: the replica's
/// child rows are streamed with only their key and referencing columns,
/// each reference is one point lookup in the parent replica, and only the
/// rows whose parent is gone are verified against the source. Parent
/// deletes and updates are ordinary binlog events, so the parent replica
/// is current; the source is read for the candidates alone, never the
/// table, and memory holds one streamed chunk at a time.
async fn reconcile_cascade_target(
    connection: &mut Conn,
    metadata: &mut MetaStore,
    database_id: &str,
    source_database: &str,
    target: &mut PollTarget,
    parents: &[CascadeParent],
) -> Result<CdcReconcileOutcome, PollError> {
    let snapshot = target.store.snapshot();
    let durable_version = metadata
        .poll_state(database_id, &target.source.name)?
        .map_or(0, |state| state.version);
    let version = snapshot
        .max_row_version()
        .unwrap_or(0)
        .max(target.store.commit_version())
        .max(durable_version)
        .checked_add(1)
        .ok_or_else(|| PollError::Decode("reconcile version exceeds UInt64".to_owned()))?;

    let projection = CascadeProjection::new(&target.source, parents);
    let column_ids = column_ids_named(&snapshot, &target.source.name, &projection.names)?;
    let key_positions = &projection.key_positions;
    let parent_positions = &projection.parent_positions;

    let mut streamed = 0_u64;
    let mut ingested = 0;
    let mut tombstones = 0;
    // Children share parents, so a parent's presence is remembered while
    // the cache stays small enough to be free.
    let mut known = vec![HashMap::<PrimaryKey, bool>::new(); parents.len()];
    if let Some((first, last)) = snapshot.key_bounds() {
        let mut chunks = ProjectedChunks::open(&snapshot, &first, &last, &column_ids)?;
        while let Some(rows) = chunks.next()? {
            let mut candidates = Vec::new();
            for values in rows {
                streamed = streamed.saturating_add(1);
                let Some(key) = key_at(&values, key_positions) else {
                    continue;
                };
                for (index, parent) in parents.iter().enumerate() {
                    let Some(parent_key) = key_at(&values, &parent_positions[index]) else {
                        continue;
                    };
                    let present = if let Some(&present) = known[index].get(&parent_key) {
                        present
                    } else {
                        let present = parent.snapshot.get(&parent_key)?.is_some();
                        if known[index].len() >= 100_000 {
                            known[index].clear();
                        }
                        known[index].insert(parent_key, present);
                        present
                    };
                    if !present {
                        candidates.push(key);
                        break;
                    }
                }
            }
            if !candidates.is_empty() {
                let (fresh, gone) = repair_candidates(
                    connection,
                    source_database,
                    target,
                    &snapshot,
                    &candidates,
                    version,
                )
                .await?;
                ingested += fresh;
                tombstones += gone;
            }
        }
    }
    if ingested > 0 || tombstones > 0 {
        target.store.checkpoint()?;
    }
    let source_count = streamed.saturating_sub(u64::try_from(tombstones).unwrap_or(u64::MAX));
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
        tombstones,
        source_count,
        version,
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
    // A cascade-affected table can be the largest in the source. Holding
    // its every row twice - the replica's and the source's - while comparing
    // them held gigabytes for minutes on a staging node, and with an
    // allocator that never returned them it was the memory the process was
    // reported to be "using". A point lookup per source row instead cost an
    // hour for two million rows, each decoding a column block. So: the
    // source is read one page at a time by key, and for keys `MySQL` and the
    // replica order alike the replica is streamed in step with it, so both
    // sides are compared by one merge that holds a page and a chunk at a
    // time. Keys the two order differently - text under a collation - keep
    // the point lookups and verify the replica's keys against the source in
    // batches afterwards; that path is slow, but nothing in it grows with
    // the table.
    let snapshot = target.store.snapshot();
    let durable_version = metadata
        .poll_state(database_id, &target.source.name)?
        .map_or(0, |state| state.version);
    let version = snapshot
        .max_row_version()
        .unwrap_or(0)
        .max(target.store.commit_version())
        .max(durable_version)
        .checked_add(1)
        .ok_or_else(|| PollError::Decode("reconcile version exceeds UInt64".to_owned()))?;
    let (ingested, tombstone_count, source_count) = if keys_order_like_mysql(&target.source) {
        reconcile_by_merge(
            connection,
            source_database,
            target,
            &snapshot,
            version,
            chunk_rows,
        )
        .await?
    } else {
        reconcile_by_lookup(
            connection,
            source_database,
            target,
            &snapshot,
            version,
            chunk_rows,
        )
        .await?
    };
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

/// Whether `MySQL`'s `ORDER BY` over the key and the replica's key order
/// agree, so a source page and a replica range can be merged. Integers
/// compare numerically on both sides and binary strings byte by byte; text
/// orders by collation in `MySQL` and by code point here, and floats reach
/// the key as text.
fn keys_order_like_mysql(table: &SourceTable) -> bool {
    table.key.columns.iter().all(|name| {
        table
            .columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
            .is_some_and(|column| {
                matches!(
                    column.pintail_type,
                    pintail_types::DataType::Boolean
                        | pintail_types::DataType::Int8
                        | pintail_types::DataType::Int16
                        | pintail_types::DataType::Int32
                        | pintail_types::DataType::Int64
                        | pintail_types::DataType::UInt8
                        | pintail_types::DataType::UInt16
                        | pintail_types::DataType::UInt32
                        | pintail_types::DataType::UInt64
                        | pintail_types::DataType::Binary
                )
            })
    })
}

/// A replica row with its key already built.
type KeyedRow = (PrimaryKey, Vec<Value>);

/// The replica streamed once in key order, handed out in step with the
/// source's pages: rows through a key on request, the rest at the end.
/// One chunk is decoded at a time and nothing is decoded twice.
struct ReplicaCursor<'a> {
    source: &'a SourceTable,
    chunks: Option<ProjectedChunks>,
    buffered: VecDeque<KeyedRow>,
}

impl<'a> ReplicaCursor<'a> {
    fn open(
        snapshot: &pintail_store::TableSnapshot,
        source: &'a SourceTable,
    ) -> Result<Self, PollError> {
        let column_ids = snapshot
            .schema()
            .columns()
            .iter()
            .map(pintail_types::Column::id)
            .collect::<Vec<_>>();
        let chunks = match snapshot.key_bounds() {
            Some((first, last)) => {
                Some(ProjectedChunks::open(snapshot, &first, &last, &column_ids)?)
            }
            None => None,
        };
        Ok(Self {
            source,
            chunks,
            buffered: VecDeque::new(),
        })
    }

    /// Buffers the next chunk; `false` once the replica is exhausted.
    fn fill(&mut self) -> Result<bool, PollError> {
        let Some(chunks) = self.chunks.as_mut() else {
            return Ok(false);
        };
        let Some(rows) = chunks.next()? else {
            self.chunks = None;
            return Ok(false);
        };
        for values in rows {
            let key = physical_key(self.source, &values)?;
            self.buffered.push_back((key, values));
        }
        Ok(true)
    }

    /// Every buffered or not-yet-decoded row whose key is at most `through`.
    fn take_through(&mut self, through: &PrimaryKey) -> Result<Vec<KeyedRow>, PollError> {
        let mut taken = Vec::new();
        loop {
            while let Some((key, _)) = self.buffered.front() {
                if key > through {
                    return Ok(taken);
                }
                taken.extend(self.buffered.pop_front());
            }
            if !self.fill()? {
                return Ok(taken);
            }
        }
    }

    /// Whatever the replica still holds past everything taken so far.
    fn next_rest(&mut self) -> Result<Option<Vec<KeyedRow>>, PollError> {
        if self.buffered.is_empty() && !self.fill()? {
            return Ok(None);
        }
        Ok(Some(self.buffered.drain(..).collect()))
    }
}

// Every repair a reconciliation ingests is a row the pass has already
// compared against the replica, so it goes through the plain ingest: the
// scan ingest's no-op suppression would look each row up again, and two
// million tombstones cost two million block decodes that way.
/// Full-row compare by merge: the replica streamed once in key order, in
/// step with the source's pages. Replica rows up to a page's last key that
/// the page lacks are gone from the source; rows the page has with other
/// content are stale; what remains in the page is new; and the replica's
/// tail past the last source key is gone. Returns (refreshed or inserted,
/// tombstoned, source rows).
async fn reconcile_by_merge(
    connection: &mut Conn,
    source_database: &str,
    target: &mut PollTarget,
    snapshot: &pintail_store::TableSnapshot,
    version: u64,
    chunk_rows: usize,
) -> Result<(usize, usize, u64), PollError> {
    let order = poll_order(&target.source, None);
    let source = target.source.clone();
    let mut replica = ReplicaCursor::open(snapshot, &source)?;
    let mut ingested = 0;
    let mut tombstones = 0;
    let mut source_count = 0_u64;
    let mut last_key: Option<PrimaryKey> = None;
    loop {
        let (condition, parameters) = keyset_condition(&target.source, last_key.as_ref());
        let rows = fetch_rows_page(
            connection,
            source_database,
            &target.source,
            &condition,
            parameters,
            &order,
            chunk_rows,
            0,
        )
        .await?;
        let fetched = rows.len();
        let mut page = BTreeMap::new();
        for row in rows {
            source_count = source_count
                .checked_add(1)
                .ok_or_else(|| PollError::Decode("source row count exceeds UInt64".to_owned()))?;
            let decoded = decode_row(&target.source, row, source_count, version, false)?;
            page.insert(decoded.key().clone(), decoded);
        }
        let Some(page_last) = page.keys().next_back().cloned() else {
            break;
        };
        let mut repairs = Vec::new();
        for (key, values) in replica.take_through(&page_last)? {
            if let Some(decoded) = page.remove(&key) {
                if decoded.values() != values.as_slice() {
                    ingested += 1;
                    repairs.push(decoded);
                }
            } else {
                tombstones += 1;
                repairs.push(StoredRow::new(key, values, version, true));
            }
        }
        ingested += page.len();
        repairs.extend(page.into_values());
        if !repairs.is_empty() {
            target.store.ingest(repairs)?;
        }
        last_key = Some(page_last);
        if fetched < chunk_rows {
            break;
        }
    }
    while let Some(rest) = replica.next_rest()? {
        let repairs = rest
            .into_iter()
            .map(|(key, values)| StoredRow::new(key, values, version, true))
            .collect::<Vec<_>>();
        tombstones += repairs.len();
        target.store.ingest(repairs)?;
    }
    Ok((ingested, tombstones, source_count))
}

/// Full-row compare for keys the two sides order differently: a point
/// lookup per source row, then the replica's keys verified against the
/// source in batches. Returns (refreshed or inserted, tombstoned, source
/// rows).
async fn reconcile_by_lookup(
    connection: &mut Conn,
    source_database: &str,
    target: &mut PollTarget,
    snapshot: &pintail_store::TableSnapshot,
    version: u64,
    chunk_rows: usize,
) -> Result<(usize, usize, u64), PollError> {
    let order = poll_order(&target.source, None);
    let mut ingested = 0;
    let mut source_count = 0_u64;
    let mut last_key: Option<PrimaryKey> = None;
    loop {
        let (condition, parameters) = keyset_condition(&target.source, last_key.as_ref());
        let rows = fetch_rows_page(
            connection,
            source_database,
            &target.source,
            &condition,
            parameters,
            &order,
            chunk_rows,
            0,
        )
        .await?;
        let fetched = rows.len();
        let mut mutations = Vec::new();
        for row in rows {
            source_count = source_count
                .checked_add(1)
                .ok_or_else(|| PollError::Decode("source row count exceeds UInt64".to_owned()))?;
            let decoded = decode_row(&target.source, row, source_count, version, false)?;
            last_key = Some(decoded.key().clone());
            if snapshot
                .get(decoded.key())?
                .is_none_or(|stored| stored.values() != decoded.values())
            {
                ingested += 1;
                mutations.push(decoded);
            }
        }
        if !mutations.is_empty() {
            target.store.ingest(mutations)?;
        }
        if fetched < chunk_rows {
            break;
        }
    }
    let tombstones =
        tombstone_absent_keys(connection, source_database, target, snapshot, version).await?;
    Ok((ingested, tombstones, source_count))
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
            // The inclusive boundary only sees rows whose cursor has kept
            // pace. Reconciliation must also repair unchanged or backdated
            // cursor values, using the same value/soft-delete comparison.
            let previous_cursor = if reconcile_requested {
                None
            } else {
                previous_cursor(previous.as_ref(), cursor)?
            };
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
        pintail_failpoint::hit("poll.after_ingest").map_err(|source| StoreError::Io {
            action: "recovery failpoint".to_owned(),
            source,
        })?;
        target.store.checkpoint()?;
    }
    pintail_failpoint::hit("poll.before_state_commit").map_err(|source| StoreError::Io {
        action: "recovery failpoint".to_owned(),
        source,
    })?;
    if reconcile_requested {
        pintail_failpoint::hit("poll.reconcile.before_state_commit").map_err(|source| {
            StoreError::Io {
                action: "recovery failpoint".to_owned(),
                source,
            }
        })?;
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
        pintail_failpoint::hit("poll.checksum.before_chunk_commit").map_err(|source| {
            StoreError::Io {
                action: "recovery failpoint".to_owned(),
                source,
            }
        })?;
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
    pintail_failpoint::hit("poll.append.after_reset").map_err(|source| StoreError::Io {
        action: "recovery failpoint".to_owned(),
        source,
    })?;
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
    let tombstones = tombstones_absent_from(
        &target.store.snapshot(),
        &target.source,
        &source_keys,
        version,
    )?;
    let count = tombstones.len();
    target.store.ingest(tombstones)?;
    Ok(count)
}

/// The `WHERE` clause and parameters that continue a key-ordered walk of a
/// source table after `last_key`, or nothing for the first page.
fn keyset_condition(
    table: &SourceTable,
    last_key: Option<&PrimaryKey>,
) -> (String, Vec<MysqlValue>) {
    let Some(key) = last_key else {
        return (String::new(), Vec::new());
    };
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
        key.parts().iter().map(key_part_mysql_value).collect(),
    )
}

/// Key-ordered projected rows of a replica range, a bounded chunk at a
/// time. The store declines to stream a small range whose rows need
/// visibility resolution and expects the caller to read it whole; that
/// case is a single chunk here, so every reconciliation pass reads a range
/// the same way whether it lives in segments or in the memtable.
enum ProjectedChunks {
    Stream(Box<pintail_store::ProjectedScanStream>),
    Whole(Option<Vec<Vec<Value>>>),
}

impl ProjectedChunks {
    fn open(
        snapshot: &pintail_store::TableSnapshot,
        start: &PrimaryKey,
        end: &PrimaryKey,
        column_ids: &[u32],
    ) -> Result<Self, PollError> {
        if let Some(stream) = snapshot.scan_projected_range_stream(start, end, column_ids)? {
            return Ok(Self::Stream(Box::new(stream)));
        }
        let scan = snapshot.scan_projected_range(start, end, column_ids)?;
        Ok(Self::Whole(Some(
            scan.into_rows()
                .into_iter()
                .map(pintail_store::ProjectedRow::into_values)
                .collect(),
        )))
    }

    fn next(&mut self) -> Result<Option<Vec<Vec<Value>>>, PollError> {
        match self {
            Self::Stream(stream) => Ok(stream
                .next_chunk(RECONCILE_SCAN_MEMORY)?
                .map(pintail_store::ProjectedValueChunk::into_rows)),
            Self::Whole(rows) => Ok(rows.take()),
        }
    }
}

/// Memory a reconciliation lets one streamed replica chunk hold.
const RECONCILE_SCAN_MEMORY: usize = 64 * 1024 * 1024;

/// Tombstones every visible replica row the source no longer holds, found
/// by streaming the replica's keys in order and asking the source about
/// each batch of them. Returns the count after ingesting.
async fn tombstone_absent_keys(
    connection: &mut Conn,
    source_database: &str,
    target: &mut PollTarget,
    snapshot: &pintail_store::TableSnapshot,
    version: u64,
) -> Result<usize, PollError> {
    let Some((first, last)) = snapshot.key_bounds() else {
        return Ok(0);
    };
    let key_columns = target.source.key.columns.clone();
    let column_ids = column_ids_named(snapshot, &target.source.name, &key_columns)?;
    let mut chunks = ProjectedChunks::open(snapshot, &first, &last, &column_ids)?;
    let positions = (0..key_columns.len()).collect::<Vec<_>>();
    let mut count = 0;
    while let Some(rows) = chunks.next()? {
        let keys = rows
            .iter()
            .filter_map(|values| key_at(values, &positions))
            .collect::<Vec<_>>();
        let (_, tombstoned) = repair_candidates(
            connection,
            source_database,
            target,
            snapshot,
            &keys,
            version,
        )
        .await?;
        count += tombstoned;
    }
    Ok(count)
}

/// Tombstones for every visible replica row whose key `source_keys` lacks,
/// found by streaming the replica in key order within a memory bound rather
/// than materializing it.
fn tombstones_absent_from(
    snapshot: &pintail_store::TableSnapshot,
    source: &SourceTable,
    source_keys: &BTreeSet<PrimaryKey>,
    version: u64,
) -> Result<Vec<StoredRow>, PollError> {
    let mut tombstones = Vec::new();
    let Some((first, last)) = snapshot.key_bounds() else {
        return Ok(tombstones);
    };
    let column_ids = snapshot
        .schema()
        .columns()
        .iter()
        .map(pintail_types::Column::id)
        .collect::<Vec<_>>();
    let mut chunks = ProjectedChunks::open(snapshot, &first, &last, &column_ids)?;
    while let Some(rows) = chunks.next()? {
        for values in rows {
            let key = physical_key(source, &values)?;
            if !source_keys.contains(&key) {
                tombstones.push(StoredRow::new(key, values, version, true));
            }
        }
    }
    Ok(tombstones)
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
        let (condition, parameters) = keyset_condition(table, last_key.as_ref());
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
            rows_are_exact: false,
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
                generation_expression: String::new(),
                extra: String::new(),
                auto_increment: false,
                default_value: None,
                default_generated: false,
                ordinal: 0,
            }],
            key: SourceKey {
                mode: KeyMode::AppendRowId,
                index_name: None,
                columns: Vec::new(),
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
