//! The cascade pass: repairs flagged child tables whose rows `MySQL` changed
//! through `ON DELETE/UPDATE CASCADE` or `SET NULL`, which the storage engine
//! applies without writing a row event for the child.
//!
//! The pass has two halves.
//!
//! **Planning** reads replica snapshots only, so it can run beside the
//! replication stream. Its input is each child's queue of parent keys: the
//! old key of every parent row the stream deleted and both keys of every
//! parent row whose key moved, queued by the stream in the same metadata
//! transaction as the checkpoint that covers them. A child row referencing
//! a queued key is a candidate: the action may have deleted it, nulled its
//! reference or moved it to a new key. Deriving candidates from the parent's
//! events rather than from parents that are now absent is what finds the
//! children of a parent key that exists again - deleted and inserted, or
//! replaced, or moved away and then reused. A sweep, run for a child the
//! first time a process repairs it, adds the children whose parent the
//! replica no longer holds, which covers parent events from before the
//! queue existed. The parent replica is read once in key order and merged
//! with the sorted references, not probed a key at a time.
//!
//! **Applying** runs inside the replication cycle, on the same open store
//! as the stream's ingest and checkpoint, so nothing writes the table
//! between the pass reading the source and the pass storing what it read.
//! Every candidate is verified against the source by its own key and either
//! refreshed from the source row or tombstoned; children the source holds
//! under a queued parent key that the replica does not hold under it are
//! fetched by that key and ingested, which brings in rows an `ON UPDATE
//! CASCADE` moved into a child's primary key. The repair is stamped above
//! every version the replica holds, and the source was read after the
//! stream's position, so a later row event, always from after that
//! position, versions above the repair and wins: the repair never
//! resurrects or removes a row an event after it decided.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
    time::Instant,
};

use chrono::Utc;
use mysql_async::{Conn, Pool};
use pintail_meta::{CascadeKeyRecord, CascadePending, MetaStore};
use pintail_probe::{ProbeReport, SourceTable};
use pintail_store::TableSnapshot;
use pintail_types::{KeyMode, PrimaryKey};

use crate::{
    CdcReconcileOutcome, CdcReconcileResult, MEMBERSHIP_BATCH, PollError, PollTarget,
    ProjectedChunks, column_ids_named, column_membership_condition, decode_row, fetch_rows_page,
    key_at, reconcile_cdc_target, repair_candidates,
};

/// A replica table as the planning half of a cascade pass reads it.
pub struct CascadeTable {
    /// The probed source table.
    pub source: SourceTable,
    /// A pinned view of its replica.
    pub snapshot: TableSnapshot,
}

/// Each named child's queue of parent keys, read at one stream position.
#[derive(Debug, Default)]
pub struct CascadeQueues(HashMap<String, CascadePending>);

/// Reads the cascade queues of `children`. Read it at the stream position
/// the planning snapshots are taken at: a key queued later belongs to a
/// later pass, whose snapshot holds the children it acted on.
///
/// # Errors
///
/// Returns an error when the metadata store cannot be read.
pub fn read_cascade_queues(
    metadata_path: &Path,
    database_id: &str,
    children: &[String],
) -> Result<CascadeQueues, PollError> {
    let metadata = MetaStore::open(metadata_path)?;
    let mut queues = HashMap::new();
    for child in children {
        queues.insert(
            child.to_ascii_lowercase(),
            metadata.cascade_pending(database_id, child)?,
        );
    }
    Ok(CascadeQueues(queues))
}

/// What the planning half decided for every due child, in repair order.
pub struct CascadePlan {
    tables: Vec<PlannedTable>,
}

impl CascadePlan {
    /// The planned tables, in repair order.
    #[must_use]
    pub fn tables(&self) -> Vec<String> {
        self.tables.iter().map(|table| table.name.clone()).collect()
    }
}

struct PlannedTable {
    name: String,
    /// The last queue position read; consumed once the repair is stored.
    through: i64,
    plan_ms: u64,
    rows_scanned: u64,
    /// Keys of parent tables in this same pass that this table was planned
    /// with: candidates of the parent's own repair, whose outcome is not
    /// known while planning. Queued again if this table's repair fails.
    chained: Vec<(String, Vec<PrimaryKey>)>,
    work: Work,
}

enum Work {
    /// Nothing queued and no sweep due: the pass costs nothing.
    Skip,
    /// The table has no source key to verify rows by.
    Keyless,
    /// Compare every row with the source.
    Full(Option<String>),
    /// Verify the candidates and fetch the queued parents' children.
    Repair {
        candidates: Vec<PrimaryKey>,
        /// Per cascading key: the child's referencing columns in the
        /// parent's key order, and the parent keys to fetch children for.
        fetch: Vec<(Vec<String>, Vec<PrimaryKey>)>,
    },
}

/// A replicated parent a child's cascading foreign key points at, resolved
/// to the parent's primary key.
struct CascadeParent<'a> {
    name: String,
    /// Referencing column names on the child, in the parent's key order.
    columns: Vec<String>,
    /// The parent's own key columns.
    key_columns: Vec<String>,
    snapshot: &'a TableSnapshot,
}

/// Whether an `ON DELETE`/`ON UPDATE` rule changes child rows without a
/// binlog row event, so `MySQL` never shows the change to a CDC reader.
fn invisible_fk_rule(rule: &str) -> bool {
    rule.eq_ignore_ascii_case("CASCADE") || rule.eq_ignore_ascii_case("SET NULL")
}

/// The parents for a targeted pass over `tables[child]`, or `None` when any
/// cascading key of the child references something a key merge cannot
/// answer: an unreplicated table, a parent without a primary key, or a
/// unique key that is not the parent's primary key. The stream queues
/// parent keys by primary key, so those children are compared in full.
fn cascade_parents(tables: &[CascadeTable], child: usize) -> Option<Vec<CascadeParent<'_>>> {
    let source = &tables[child].source;
    let mut parents = Vec::new();
    for key in source
        .foreign_keys
        .iter()
        .filter(|key| invisible_fk_rule(&key.delete_rule) || invisible_fk_rule(&key.update_rule))
    {
        let parent = tables.iter().find(|table| {
            table
                .source
                .name
                .eq_ignore_ascii_case(&key.referenced_table)
        })?;
        if parent.source.key.mode == KeyMode::AppendRowId
            || parent.source.key.columns.len() != key.referenced_columns.len()
        {
            return None;
        }
        let mut columns = Vec::with_capacity(key.columns.len());
        for parent_column in &parent.source.key.columns {
            let position = key
                .referenced_columns
                .iter()
                .position(|referenced| referenced.eq_ignore_ascii_case(parent_column))?;
            columns.push(key.columns[position].clone());
        }
        parents.push(CascadeParent {
            name: parent.source.name.clone(),
            columns,
            key_columns: parent.source.key.columns.clone(),
            snapshot: &parent.snapshot,
        });
    }
    (!parents.is_empty()).then_some(parents)
}

/// Orders the named tables so a parent is repaired before the children
/// that reference it: a cascade removes rows at every level, and a child's
/// candidates include the rows its parent's repair touches. A cycle keeps
/// the remaining names in their given order.
pub(crate) fn cascade_order(names: &[String], sources: &[&SourceTable]) -> Vec<String> {
    let mut remaining = names.to_vec();
    let mut ordered = Vec::with_capacity(names.len());
    while !remaining.is_empty() {
        let independent = remaining.iter().position(|name| {
            sources
                .iter()
                .find(|source| source.name.eq_ignore_ascii_case(name))
                .is_none_or(|source| {
                    !source.foreign_keys.iter().any(|key| {
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

/// The flagged child tables with a cascading foreign key on `parent`.
fn flagged_children<'a>(
    sources: impl Iterator<Item = &'a SourceTable>,
    parent: &str,
) -> Vec<String> {
    sources
        .filter(|child| {
            child.requires_reconciliation
                && child.foreign_keys.iter().any(|key| {
                    key.referenced_table.eq_ignore_ascii_case(parent)
                        && (invisible_fk_rule(&key.delete_rule)
                            || invisible_fk_rule(&key.update_rule))
                })
        })
        .map(|child| child.name.clone())
        .collect()
}

/// Distinct references a sweep holds before resolving them against the
/// parent: one ordered read of the parent's keys per this many.
const SWEEP_BATCH: usize = 500_000;

/// Candidates past which a child is compared in full instead: reading the
/// whole child once costs less than verifying millions of keys in batches.
const CANDIDATE_CAP: usize = 2_000_000;

/// Plans one cascade pass over `due`, reading replica snapshots only.
/// Children named in `sweep` also take the children whose parent the
/// replica no longer holds.
#[must_use]
pub fn plan_cascade_pass(
    tables: &[CascadeTable],
    queues: &CascadeQueues,
    due: &[String],
    sweep: &BTreeSet<String>,
) -> CascadePlan {
    let sources = tables.iter().map(|table| &table.source).collect::<Vec<_>>();
    // Candidates of each table planned so far, by lower-case name: the rows
    // its repair may tombstone or refresh, and so keys its own flagged
    // children must look at in this same pass.
    let mut chained: HashMap<String, HashSet<PrimaryKey>> = HashMap::new();
    let mut planned = Vec::new();
    for name in cascade_order(due, &sources) {
        let started = Instant::now();
        let pending = queues
            .0
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default();
        let sweeping = sweep.iter().any(|swept| swept.eq_ignore_ascii_case(&name));
        let mut table = PlannedTable {
            name: name.clone(),
            through: pending.through,
            plan_ms: 0,
            rows_scanned: 0,
            chained: Vec::new(),
            work: Work::Skip,
        };
        match tables
            .iter()
            .position(|table| table.source.name.eq_ignore_ascii_case(&name))
        {
            None => {
                table.work = Work::Full(Some(format!("{name} is not among the opened targets")));
            }
            Some(index) => {
                plan_table(tables, index, &pending, sweeping, &chained, &mut table);
            }
        }
        if let Work::Repair { candidates, .. } = &table.work {
            chained.insert(
                name.to_ascii_lowercase(),
                candidates.iter().cloned().collect(),
            );
        }
        table.plan_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        planned.push(table);
    }
    CascadePlan { tables: planned }
}

fn plan_table(
    tables: &[CascadeTable],
    index: usize,
    pending: &CascadePending,
    sweeping: bool,
    chained: &HashMap<String, HashSet<PrimaryKey>>,
    table: &mut PlannedTable,
) {
    let source = &tables[index].source;
    let queued = !pending.keys.is_empty() || pending.full;
    if source.key.mode == KeyMode::AppendRowId {
        table.work = if queued || sweeping {
            Work::Keyless
        } else {
            Work::Skip
        };
        return;
    }
    if pending.full {
        table.work = Work::Full(Some("its queue of parent keys outgrew its cap".to_owned()));
        return;
    }
    let Some(parents) = cascade_parents(tables, index) else {
        table.work = Work::Full(None);
        return;
    };
    // Queued keys and the keys of parents repaired earlier in this pass, per
    // cascading key.
    let mut keys = vec![HashSet::<PrimaryKey>::new(); parents.len()];
    for (parent_name, key_json) in &pending.keys {
        let Ok(key) = serde_json::from_str::<PrimaryKey>(key_json) else {
            continue;
        };
        for (position, parent) in parents.iter().enumerate() {
            if parent.name.eq_ignore_ascii_case(parent_name) {
                keys[position].insert(key.clone());
            }
        }
    }
    for (position, parent) in parents.iter().enumerate() {
        if let Some(upstream) = chained.get(&parent.name.to_ascii_lowercase())
            && !upstream.is_empty()
        {
            keys[position].extend(upstream.iter().cloned());
            if !table
                .chained
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(&parent.name))
            {
                table
                    .chained
                    .push((parent.name.clone(), upstream.iter().cloned().collect()));
            }
        }
    }
    if keys.iter().all(HashSet::is_empty) && !sweeping {
        table.work = Work::Skip;
        return;
    }
    match scan_candidates(&tables[index], &parents, &mut keys, sweeping) {
        Ok((candidates, scanned)) => {
            table.rows_scanned = scanned;
            if candidates.len() > CANDIDATE_CAP {
                table.work = Work::Full(Some(format!(
                    "{} candidates exceed the targeted repair's cap",
                    candidates.len()
                )));
                return;
            }
            // Children are fetched by the parent keys the queue and the
            // upstream repairs name - not by the absent ones a sweep found,
            // whose children at the source are orphans the replica holds.
            let mut fetch = Vec::new();
            let mut queued_keys = vec![HashSet::<PrimaryKey>::new(); parents.len()];
            for (parent_name, key_json) in &pending.keys {
                if let Ok(key) = serde_json::from_str::<PrimaryKey>(key_json) {
                    for (position, parent) in parents.iter().enumerate() {
                        if parent.name.eq_ignore_ascii_case(parent_name) {
                            queued_keys[position].insert(key.clone());
                        }
                    }
                }
            }
            for (position, parent) in parents.iter().enumerate() {
                if let Some(upstream) = chained.get(&parent.name.to_ascii_lowercase()) {
                    queued_keys[position].extend(upstream.iter().cloned());
                }
                let mut wanted = std::mem::take(&mut queued_keys[position])
                    .into_iter()
                    .collect::<Vec<_>>();
                if !wanted.is_empty() {
                    wanted.sort();
                    fetch.push((parent.columns.clone(), wanted));
                }
            }
            table.work = Work::Repair { candidates, fetch };
        }
        // A referencing column the replica no longer has (an ALTER the
        // probe has not seen yet) cannot be scanned: compare the table in
        // full, which needs no foreign-key columns at all.
        Err(error) => {
            table.work = Work::Full(Some(format!("the targeted scan failed: {error}")));
        }
    }
}

/// The child's key and every referencing column, in one projection, with
/// the positions each key is built from.
struct CascadeProjection {
    names: Vec<String>,
    key_positions: Vec<usize>,
    parent_positions: Vec<Vec<usize>>,
}

impl CascadeProjection {
    fn new(source: &SourceTable, parents: &[CascadeParent<'_>]) -> Self {
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

/// Streams the child's key and referencing columns. A sweep reads them
/// twice: first to gather the distinct references and resolve which parents
/// the replica lacks - by merging the sorted references with one key-ordered
/// read of the parent, never a lookup per key - and then to collect the
/// rows referencing a queued or absent parent. Returns the candidates and
/// the rows read.
fn scan_candidates(
    child: &CascadeTable,
    parents: &[CascadeParent<'_>],
    keys: &mut [HashSet<PrimaryKey>],
    sweeping: bool,
) -> Result<(Vec<PrimaryKey>, u64), PollError> {
    let snapshot = &child.snapshot;
    let Some((first, last)) = snapshot.key_bounds() else {
        return Ok((Vec::new(), 0));
    };
    let projection = CascadeProjection::new(&child.source, parents);
    let column_ids = column_ids_named(snapshot, &child.source.name, &projection.names)?;
    let mut scanned = 0_u64;
    if sweeping {
        let mut references = vec![BTreeSet::<PrimaryKey>::new(); parents.len()];
        let mut chunks = ProjectedChunks::open(snapshot, &first, &last, &column_ids)?;
        while let Some(rows) = chunks.next()? {
            for values in rows {
                scanned = scanned.saturating_add(1);
                for (position, parent) in parents.iter().enumerate() {
                    let Some(reference) = key_at(&values, &projection.parent_positions[position])
                    else {
                        continue;
                    };
                    if keys[position].contains(&reference) {
                        continue;
                    }
                    references[position].insert(reference);
                    if references[position].len() >= SWEEP_BATCH {
                        let batch = std::mem::take(&mut references[position]);
                        keys[position].extend(absent_parents(parent, &batch)?);
                    }
                }
            }
        }
        for (position, parent) in parents.iter().enumerate() {
            let batch = std::mem::take(&mut references[position]);
            keys[position].extend(absent_parents(parent, &batch)?);
        }
    }
    let mut candidates = Vec::new();
    if keys.iter().all(HashSet::is_empty) {
        return Ok((candidates, scanned));
    }
    let mut chunks = ProjectedChunks::open(snapshot, &first, &last, &column_ids)?;
    let mut second = 0_u64;
    while let Some(rows) = chunks.next()? {
        for values in rows {
            second = second.saturating_add(1);
            let Some(key) = key_at(&values, &projection.key_positions) else {
                continue;
            };
            let hit = parents.iter().enumerate().any(|(position, _)| {
                key_at(&values, &projection.parent_positions[position])
                    .is_some_and(|reference| keys[position].contains(&reference))
            });
            if hit {
                candidates.push(key);
            }
        }
    }
    Ok((candidates, scanned.max(second)))
}

/// The keys of `wanted` the parent replica does not hold, by merging the
/// sorted keys with a key-ordered read of the parent's key columns over
/// their range.
fn absent_parents(
    parent: &CascadeParent<'_>,
    wanted: &BTreeSet<PrimaryKey>,
) -> Result<Vec<PrimaryKey>, PollError> {
    let (Some(first), Some(last)) = (wanted.first(), wanted.last()) else {
        return Ok(Vec::new());
    };
    let column_ids = column_ids_named(parent.snapshot, &parent.name, &parent.key_columns)?;
    let positions = (0..parent.key_columns.len()).collect::<Vec<_>>();
    let mut wanted = wanted.iter().peekable();
    let mut absent = Vec::new();
    let mut chunks = ProjectedChunks::open(parent.snapshot, first, last, &column_ids)?;
    while let Some(rows) = chunks.next()? {
        for values in rows {
            let Some(present) = key_at(&values, &positions) else {
                continue;
            };
            while let Some(next) = wanted.peek() {
                match (*next).cmp(&present) {
                    std::cmp::Ordering::Less => {
                        absent.push((*next).clone());
                        wanted.next();
                    }
                    std::cmp::Ordering::Equal => {
                        wanted.next();
                        break;
                    }
                    std::cmp::Ordering::Greater => break,
                }
            }
            if wanted.peek().is_none() {
                return Ok(absent);
            }
        }
    }
    absent.extend(wanted.cloned());
    Ok(absent)
}

/// Applies a plan inside the replication cycle, on the stores the cycle
/// holds. A table's failure is its own: it is reported in its outcome, its
/// queue is kept for the next pass, and the tables after it are repaired.
///
/// # Errors
///
/// Returns an error only when the source or the metadata store cannot be
/// reached at all.
#[allow(clippy::too_many_lines)]
pub async fn apply_cascade_plan(
    pool: &Pool,
    metadata_path: &Path,
    database_id: &str,
    report: &ProbeReport,
    mut targets: Vec<PollTarget>,
    plan: CascadePlan,
    chunk_rows: usize,
) -> Result<CdcReconcileResult, PollError> {
    targets.sort_by(|left, right| left.source.name.cmp(&right.source.name));
    let mut metadata = MetaStore::open(metadata_path)?;
    let mut connection = pool.get_conn().await?;
    let mut outcomes = Vec::with_capacity(plan.tables.len());
    // Which planned tables were planned with keys chained from which parent:
    // their parent's repair need not queue those keys for them again.
    let chained_into =
        plan.tables
            .iter()
            .filter(|table| matches!(table.work, Work::Repair { .. }))
            .flat_map(|table| {
                table.chained.iter().map(|(parent, _)| {
                    (parent.to_ascii_lowercase(), table.name.to_ascii_lowercase())
                })
            })
            .collect::<HashSet<_>>();
    let mut failed = HashSet::new();
    for planned in &plan.tables {
        let started = Instant::now();
        let mut outcome = CdcReconcileOutcome {
            table: planned.name.clone(),
            ingested: 0,
            tombstones: 0,
            source_count: 0,
            rows_compared: planned.rows_scanned,
            source_verified: 0,
            elapsed_ms: 0,
            version: 0,
            skipped: false,
            resync: false,
            error: None,
        };
        let result = apply_table(
            &mut connection,
            &mut metadata,
            database_id,
            report,
            &mut targets,
            planned,
            &chained_into,
            &failed,
            chunk_rows,
            &mut outcome,
        )
        .await;
        if let Err(error) = result {
            // Keys chained from an upstream repair exist only in this plan:
            // queued again so the next pass still sees them.
            let records = planned
                .chained
                .iter()
                .flat_map(|(parent, keys)| {
                    keys.iter().filter_map(|key| {
                        serde_json::to_string(key)
                            .ok()
                            .map(|key_json| CascadeKeyRecord {
                                child: planned.name.clone(),
                                parent: parent.clone(),
                                key_json,
                            })
                    })
                })
                .collect::<Vec<_>>();
            let _ = metadata.queue_cascade_keys(database_id, &records);
            pintail_log::log_error!(
                "cascade reconcile db={database_id} table={} failed: {error}",
                planned.name
            );
            failed.insert(planned.name.to_ascii_lowercase());
            outcome.error = Some(error.to_string());
        }
        outcome.elapsed_ms = planned
            .plan_ms
            .saturating_add(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        outcomes.push(outcome);
    }
    Ok(CdcReconcileResult {
        tables: outcomes,
        targets,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn apply_table(
    connection: &mut Conn,
    metadata: &mut MetaStore,
    database_id: &str,
    report: &ProbeReport,
    targets: &mut [PollTarget],
    planned: &PlannedTable,
    chained_into: &HashSet<(String, String)>,
    failed: &HashSet<String>,
    chunk_rows: usize,
    outcome: &mut CdcReconcileOutcome,
) -> Result<(), PollError> {
    let name = &planned.name;
    let consume = |metadata: &MetaStore| -> Result<(), PollError> {
        if planned.through > 0 {
            metadata.consume_cascade_pending(database_id, name, planned.through)?;
        }
        Ok(())
    };
    let children = flagged_children(report.tables.iter(), name);
    match &planned.work {
        Work::Skip => {
            // Nothing to read, but the pass happened: the schedule counts
            // from it.
            let state = metadata.poll_state(database_id, name)?;
            metadata.commit_cdc_reconciliation(
                database_id,
                name,
                state.as_ref().map_or(0, |state| state.source_count),
                state.as_ref().map_or(0, |state| state.version),
                &Utc::now().to_rfc3339(),
            )?;
            outcome.skipped = true;
            consume(metadata)?;
        }
        Work::Keyless => {
            // Without a key no row can be verified or replaced on its own,
            // and a row-level diff cannot tell an invisible action from an
            // insert the stream has not applied yet. A fresh copy of the
            // table is the repair: the same one a quarantined table gets,
            // which hands the copy to the stream at a consistent position.
            let reason = "a foreign-key action at the source may have changed this table, \
                          which has no primary or unique key to repair rows by; recopying it";
            metadata.mark_table_needs_resync(database_id, name, reason)?;
            let state = metadata.poll_state(database_id, name)?;
            metadata.commit_cdc_reconciliation(
                database_id,
                name,
                state.as_ref().map_or(0, |state| state.source_count),
                state.as_ref().map_or(0, |state| state.version),
                &Utc::now().to_rfc3339(),
            )?;
            pintail_log::log_info!("cascade reconcile db={database_id} table={name}: {reason}");
            outcome.resync = true;
            consume(metadata)?;
        }
        Work::Full(reason) => {
            if let Some(reason) = reason {
                pintail_log::log_info!(
                    "cascade reconcile db={database_id} table={name} compared in full: {reason}"
                );
            }
            if !report
                .tables
                .iter()
                .any(|source| source.name.eq_ignore_ascii_case(name))
            {
                return Err(PollError::InvalidConfiguration(format!(
                    "target {name} is absent from the probe report"
                )));
            }
            let index = target_index(targets, name)?;
            let full = reconcile_cdc_target(
                connection,
                metadata,
                database_id,
                &report.database,
                &mut targets[index],
                chunk_rows,
            )
            .await?;
            outcome.ingested = full.ingested;
            outcome.tombstones = full.tombstones;
            outcome.source_count = full.source_count;
            outcome.rows_compared = full.rows_compared;
            outcome.source_verified = full.source_verified;
            outcome.version = full.version;
            consume(metadata)?;
            // Which rows changed is not known here, so the flagged tables
            // below this one are compared in full on their next pass.
            if full.ingested + full.tombstones > 0 {
                let records = children
                    .iter()
                    .map(|child| CascadeKeyRecord {
                        child: child.clone(),
                        parent: name.clone(),
                        key_json: String::new(),
                    })
                    .collect::<Vec<_>>();
                metadata.queue_cascade_keys(database_id, &records)?;
            }
        }
        Work::Repair { candidates, fetch } => {
            let index = target_index(targets, name)?;
            let target = &mut targets[index];
            let snapshot = target.store.snapshot();
            let state = metadata.poll_state(database_id, &target.source.name)?;
            let durable_version = state.as_ref().map_or(0, |state| state.version);
            let prior_count = state.as_ref().map_or(0, |state| state.source_count);
            let version = snapshot
                .max_row_version()
                .unwrap_or(0)
                .max(target.store.commit_version())
                .max(durable_version)
                .checked_add(1)
                .ok_or_else(|| PollError::Decode("reconcile version exceeds UInt64".to_owned()))?;
            let mut written = Vec::new();
            let (refreshed, tombstoned) = repair_candidates(
                connection,
                &report.database,
                target,
                &snapshot,
                candidates,
                version,
                Some(&mut written),
            )
            .await?;
            let mut verified = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
            // Children the source holds under a queued parent key that the
            // replica does not hold as candidates: rows an `ON UPDATE
            // CASCADE` moved into the child's own key, the old key of which
            // was a candidate above.
            let known = candidates.iter().collect::<HashSet<_>>();
            let mut fetched = HashSet::new();
            let mut inserted = 0;
            for (columns, keys) in fetch {
                for batch in keys.chunks(MEMBERSHIP_BATCH / 5) {
                    let (condition, parameters) = column_membership_condition(columns, batch);
                    let rows = fetch_rows_page(
                        connection,
                        &report.database,
                        &target.source,
                        &condition,
                        parameters,
                        "",
                        usize::MAX,
                        0,
                    )
                    .await?;
                    let mut repairs = Vec::new();
                    for row in rows {
                        verified = verified.saturating_add(1);
                        let decoded = decode_row(&target.source, row, 0, version, false)?;
                        if known.contains(decoded.key()) || !fetched.insert(decoded.key().clone()) {
                            continue;
                        }
                        written.push(decoded.key().clone());
                        repairs.push(decoded);
                    }
                    if !repairs.is_empty() {
                        inserted += repairs.len();
                        target.store.ingest(repairs)?;
                    }
                }
            }
            let ingested = refreshed + inserted;
            if ingested > 0 || tombstoned > 0 {
                target.store.checkpoint()?;
            }
            // A targeted pass reads the table only when it sweeps; otherwise
            // the count the last full read found stands.
            let rows = if planned.rows_scanned > 0 {
                planned
                    .rows_scanned
                    .saturating_sub(u64::try_from(tombstoned).unwrap_or(u64::MAX))
            } else {
                prior_count
            };
            metadata.commit_cdc_reconciliation(
                database_id,
                &target.source.name,
                rows,
                version,
                &Utc::now().to_rfc3339(),
            )?;
            consume(metadata)?;
            // The rows this repair wrote are parent rows for the flagged
            // tables below this one. A table planned in this pass with this
            // table's candidates already looks at all of them but the ones
            // fetched by parent key; the rest are queued.
            let lower = name.to_ascii_lowercase();
            let mut records = Vec::new();
            for child in &children {
                let child_lower = child.to_ascii_lowercase();
                let covered = chained_into.contains(&(lower.clone(), child_lower.clone()))
                    && !failed.contains(&child_lower);
                for key in &written {
                    if covered && known.contains(key) {
                        continue;
                    }
                    records.push(CascadeKeyRecord {
                        child: child.clone(),
                        parent: name.clone(),
                        key_json: serde_json::to_string(key)?,
                    });
                }
            }
            metadata.queue_cascade_keys(database_id, &records)?;
            outcome.ingested = ingested;
            outcome.tombstones = tombstoned;
            outcome.source_count = rows;
            outcome.source_verified = verified;
            outcome.version = version;
        }
    }
    Ok(())
}

fn target_index(targets: &[PollTarget], name: &str) -> Result<usize, PollError> {
    targets
        .iter()
        .position(|target| target.source.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            PollError::InvalidConfiguration(format!(
                "reconciliation target {name} is not among the opened targets"
            ))
        })
}
