//! Change capture against a reference model, in process.
//!
//! A seeded generator plays the source: typed inserts, updates that keep or
//! move the key, deletes, multi-table transactions large enough to spill,
//! added columns and truncates. Every transaction goes through the real apply
//! path - `stage_row_change`, `commit_pending`, `apply_column_change`,
//! `truncate_target` - and between transactions the store is flushed,
//! compacted, reclaimed or reopened. A crash stops the apply at one recovery
//! point; the restart reopens every table from its tracked schema, resumes at
//! the durable checkpoint and replays the source log from there, as a
//! reconnecting stream would. After every step each table must hold exactly
//! the rows the source holds.
//!
//! Row images are handed over decoded, so the binlog decoder is outside the
//! simulation; everything from a decoded row to a durable checkpoint is
//! inside it. `PINTAIL_CDC_SIM_SEEDS`, `PINTAIL_CDC_SIM_SEED_BASE` and `PINTAIL_CDC_SIM_STEPS`
//! widen a run;
//! `PINTAIL_CDC_SIM_SEED` replays one seed.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use pintail_meta::{MetaStore, SnapshotCheckpointRecord};
use pintail_probe::{SourceColumn, SourceFlavor, SourceKey, SourceTable};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, Value};

use crate::{
    CdcError, CdcTarget, GtidIdentity, PendingTransaction, StreamPosition, apply_column_change,
    commit_pending, stage_row_change, truncate_target,
};

thread_local! {
    static ARMED: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Fails the apply once at `site` when the simulation armed it.
pub(crate) fn crash_if_armed(site: &'static str) -> Result<(), CdcError> {
    if ARMED.with(Cell::get) == Some(site) {
        ARMED.with(|armed| armed.set(None));
        return Err(CdcError::Decode(format!("{CRASH_MARKER} {site}")));
    }
    Ok(())
}

const CRASH_MARKER: &str = "simulated crash at";
const DATABASE: &str = "sim";
const SID: [u8; 16] = [0x5a; 16];
const BINLOG_FILE: &str = "mysql-bin.000001";
const RECOVERY_POINTS: [&str; 6] = [
    "cdc.after_ingest",
    "cdc.after_first_table_sync",
    "cdc.before_checkpoint_commit",
    "cdc.after_checkpoint_commit",
    "cdc.ddl.after_history",
    "cdc.ddl.after_evolve",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Gtid,
    FilePosition,
}

#[derive(Clone, Debug)]
struct RowOp {
    table: usize,
    before: Option<Vec<Value>>,
    after: Option<Vec<Value>>,
}

#[derive(Clone, Debug)]
enum Change {
    Rows(Vec<RowOp>),
    AddColumn(usize),
    Truncate(usize),
}

#[derive(Clone, Debug)]
struct Transaction {
    sequence: u64,
    change: Change,
}

impl Transaction {
    /// The binlog position of the transaction's commit event. Events inside
    /// it sit below this, and the next transaction starts above it.
    fn commit_position(&self) -> u64 {
        self.sequence * 100_000 + 99_999
    }
}

/// What the source holds for one table.
struct TableModel {
    keyed: bool,
    rows: BTreeMap<PrimaryKey, Vec<Value>>,
    appended: Vec<Vec<Value>>,
}

fn column(id: u32, name: &str, pintail_type: DataType, nullable: bool) -> SourceColumn {
    let (data, column) = match pintail_type {
        DataType::Utf8 => ("varchar", "varchar(64)"),
        _ => ("bigint", "bigint"),
    };
    SourceColumn {
        id,
        name: name.to_owned(),
        mysql_data_type: data.to_owned(),
        mysql_column_type: column.to_owned(),
        pintail_type,
        nullable,
        character_set: None,
        collation: None,
        generated_stored: false,
        generation_expression: String::new(),
        // The simulation builds this column rather than probing for it, so
        // an empty expression here means the column has none.
        generation_captured: true,
        extra: String::new(),
        auto_increment: false,
        default_value: None,
        default_generated: false,
        ordinal: id.saturating_sub(1),
    }
}

fn table(name: &str, columns: Vec<SourceColumn>, key: &[&str]) -> SourceTable {
    let mode = if key.is_empty() {
        KeyMode::AppendRowId
    } else {
        KeyMode::Primary
    };
    SourceTable {
        name: name.to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(0),
        rows_are_exact: true,
        source_column_count: u32::try_from(columns.len()).expect("column count"),
        columns,
        key: SourceKey {
            mode,
            index_name: (!key.is_empty()).then(|| "PRIMARY".to_owned()),
            columns: key.iter().map(|&column| column.to_owned()).collect(),
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        foreign_keys: Vec::new(),
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
    }
}

/// A keyed table, a composite-keyed table and a keyless one.
fn initial_sources() -> Vec<SourceTable> {
    vec![
        table(
            "orders",
            vec![
                column(1, "id", DataType::Int64, false),
                column(2, "qty", DataType::Int64, true),
                column(3, "note", DataType::Utf8, true),
            ],
            &["id"],
        ),
        table(
            "lines",
            vec![
                column(1, "order_id", DataType::Int64, false),
                column(2, "line", DataType::Utf8, false),
                column(3, "amount", DataType::Int64, true),
            ],
            &["order_id", "line"],
        ),
        table(
            "events",
            vec![
                column(1, "kind", DataType::Utf8, true),
                column(2, "n", DataType::Int64, true),
            ],
            &[],
        ),
    ]
}

fn store_options(rng: &mut Rng) -> StoreOptions {
    StoreOptions {
        // Small memtables flush inside ingest as well as on request.
        memtable_bytes: [1 << 12, 1 << 16, 1 << 22][usize::try_from(rng.below(3)).expect("index")],
        compaction_fan_in: 2,
        background_compaction: false,
        ..StoreOptions::default()
    }
}

struct Simulation {
    seed: u64,
    rng: Rng,
    mode: Mode,
    directory: PathBuf,
    metadata_path: PathBuf,
    options: StoreOptions,
    maximum_bytes: usize,
    sources: Vec<SourceTable>,
    models: Vec<TableModel>,
    metadata: MetaStore,
    targets: Vec<CdcTarget>,
    blocked: BTreeSet<usize>,
    position: StreamPosition,
    pending: PendingTransaction,
    log: Vec<Transaction>,
    trace: Vec<String>,
}

impl Simulation {
    fn new(seed: u64, directory: &Path) -> Self {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mode = if rng.chance(50) {
            Mode::Gtid
        } else {
            Mode::FilePosition
        };
        let options = store_options(&mut rng);
        // A few kilobytes forces most multi-row transactions to spill.
        let maximum_bytes = if rng.chance(30) { 1 << 12 } else { 1 << 24 };
        let metadata_path = directory.join("pintail-meta.db");
        let metadata = MetaStore::open(&metadata_path).expect("simulation metadata");
        metadata
            .upsert_database(DATABASE, "app", b"unused", "2026-09-13T00:00:00Z")
            .expect("simulation database");
        let sources = initial_sources();
        let mut targets = Vec::new();
        let mut models = Vec::new();
        for source in &sources {
            metadata
                .upsert_snapshot_table(DATABASE, &source.name, None, None)
                .expect("simulation table");
            let store = TableStore::open(
                directory.join(&source.name),
                source.table_schema().expect("simulation schema"),
                options,
            )
            .expect("simulation store");
            targets.push(CdcTarget::new(source.clone(), store).expect("simulation target"));
            models.push(TableModel {
                keyed: source.key.mode != KeyMode::AppendRowId,
                rows: BTreeMap::new(),
                appended: Vec::new(),
            });
        }
        let position =
            StreamPosition::from_checkpoint(initial_checkpoint(mode), SourceFlavor::Mysql)
                .expect("simulation position");
        Self {
            seed,
            rng,
            mode,
            directory: directory.to_path_buf(),
            metadata_path,
            options,
            maximum_bytes,
            sources,
            models,
            metadata,
            targets,
            blocked: BTreeSet::new(),
            position,
            pending: PendingTransaction::default(),
            log: Vec::new(),
            trace: Vec::new(),
        }
    }

    fn fail(&self, step: usize, message: &str) -> ! {
        let mut report = format!(
            "cdc simulation seed={} mode={:?} step={step}: {message}\nlast operations:\n",
            self.seed, self.mode
        );
        for line in self.trace.iter().rev().take(16).rev() {
            let _ = writeln!(report, "  {line}");
        }
        panic!("{report}");
    }

    fn value(&mut self, data_type: DataType, nullable: bool) -> Value {
        if nullable && self.rng.chance(15) {
            return Value::Null;
        }
        match data_type {
            DataType::Utf8 => {
                let words = [
                    "",
                    "a",
                    "A",
                    "ä",
                    "mixed Case",
                    "trailing ",
                    "ß",
                    "long-text-value",
                ];
                Value::Utf8(
                    words[usize::try_from(self.rng.below(words.len() as u64)).expect("index")]
                        .to_owned(),
                )
            }
            _ => Value::Int64(i64::try_from(self.rng.below(2_000)).expect("small") - 1_000),
        }
    }

    fn key_values(&mut self, table: usize) -> Vec<Value> {
        match table {
            0 => vec![Value::Int64(
                i64::try_from(self.rng.below(48)).expect("small"),
            )],
            _ => vec![
                Value::Int64(i64::try_from(self.rng.below(6)).expect("small")),
                Value::Utf8(
                    ["a", "b", "c", "d"][usize::try_from(self.rng.below(4)).expect("index")]
                        .to_owned(),
                ),
            ],
        }
    }

    fn model_key(&self, table: usize, values: &[Value]) -> PrimaryKey {
        let source = &self.sources[table];
        let parts = source
            .key
            .columns
            .iter()
            .map(|name| {
                let index = source
                    .columns
                    .iter()
                    .position(|column| &column.name == name)
                    .expect("key column");
                match &values[index] {
                    Value::Int64(value) => KeyPart::Int64(*value),
                    Value::Utf8(value) => KeyPart::Utf8(value.clone()),
                    other => panic!("unexpected key value {other:?}"),
                }
            })
            .collect();
        PrimaryKey::new(parts).expect("model key")
    }

    /// A full row for `table` with the given key values and random payload.
    fn row(&mut self, table: usize, key: &[Value]) -> Vec<Value> {
        let columns = self.sources[table].columns.clone();
        let key_width = key.len();
        columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                if index < key_width {
                    key[index].clone()
                } else {
                    self.value(column.pintail_type, column.nullable)
                }
            })
            .collect()
    }

    fn generate_rows(&mut self) -> Change {
        let count = if self.rng.chance(5) {
            200 + self.rng.below(400)
        } else {
            1 + self.rng.below(12)
        };
        let mut ops = Vec::new();
        for _ in 0..count {
            let table = usize::try_from(self.rng.below(3)).expect("table");
            if !self.models[table].keyed {
                let after = self.row(table, &[]);
                self.models[table].appended.push(after.clone());
                ops.push(RowOp {
                    table,
                    before: None,
                    after: Some(after),
                });
                continue;
            }
            let existing = self.models[table].rows.len() as u64;
            let roll = self.rng.below(100);
            if existing == 0 || roll < 45 {
                let key = self.key_values(table);
                let row = self.row(table, &key);
                let model_key = self.model_key(table, &row);
                if self.models[table].rows.contains_key(&model_key) {
                    continue;
                }
                self.models[table].rows.insert(model_key, row.clone());
                ops.push(RowOp {
                    table,
                    before: None,
                    after: Some(row),
                });
            } else {
                let pick = usize::try_from(self.rng.below(existing)).expect("pick");
                let (model_key, before) = self.models[table]
                    .rows
                    .iter()
                    .nth(pick)
                    .map(|(key, row)| (key.clone(), row.clone()))
                    .expect("existing row");
                if roll < 75 {
                    let key_width = self.sources[table].key.columns.len();
                    let key = if self.rng.chance(25) {
                        self.key_values(table)
                    } else {
                        before[..key_width].to_vec()
                    };
                    let after = self.row(table, &key);
                    let after_key = self.model_key(table, &after);
                    if after_key != model_key && self.models[table].rows.contains_key(&after_key) {
                        continue;
                    }
                    self.models[table].rows.remove(&model_key);
                    self.models[table].rows.insert(after_key, after.clone());
                    ops.push(RowOp {
                        table,
                        before: Some(before),
                        after: Some(after),
                    });
                } else {
                    self.models[table].rows.remove(&model_key);
                    ops.push(RowOp {
                        table,
                        before: Some(before),
                        after: None,
                    });
                }
            }
        }
        Change::Rows(ops)
    }

    fn generate_add_column(&mut self) -> Option<Change> {
        let table = usize::try_from(self.rng.below(3)).expect("table");
        if self.sources[table].columns.len() >= 7 {
            return None;
        }
        let id = self.sources[table]
            .columns
            .iter()
            .map(|c| c.id)
            .max()
            .unwrap_or(0)
            + 1;
        let data_type = if self.rng.chance(50) {
            DataType::Utf8
        } else {
            DataType::Int64
        };
        self.sources[table]
            .columns
            .push(column(id, &format!("added_{id}"), data_type, true));
        self.sources[table].source_column_count += 1;
        let model = &mut self.models[table];
        for row in model.rows.values_mut().chain(model.appended.iter_mut()) {
            row.push(Value::Null);
        }
        Some(Change::AddColumn(table))
    }

    fn generate_truncate(&mut self) -> Change {
        let table = usize::try_from(self.rng.below(3)).expect("table");
        self.models[table].rows.clear();
        self.models[table].appended.clear();
        Change::Truncate(table)
    }

    fn open_transaction(&mut self, transaction: &Transaction) {
        if self.mode == Mode::Gtid {
            self.position.pending_gtid = Some(GtidIdentity {
                sid: SID,
                tag: None,
                sequence: transaction.sequence,
            });
        }
        self.pending = PendingTransaction::default();
    }

    /// Applies one source transaction through the real apply path.
    fn deliver(&mut self, index: usize) -> Result<(), CdcError> {
        let transaction = self.log[index].clone();
        self.open_transaction(&transaction);
        let base = transaction.sequence * 100_000;
        let statement = format!("-- simulated transaction {}", transaction.sequence);
        match &transaction.change {
            Change::Rows(ops) => {
                for (offset, op) in ops.iter().enumerate() {
                    if self.blocked.contains(&op.table) {
                        continue;
                    }
                    // A row image older than the table's current shape is
                    // narrower; the decoder aligns it by padding the columns
                    // it lacks, which is what replay after a schema change
                    // sees.
                    let width = self.targets[op.table].source.columns.len();
                    let pad = |image: &Option<Vec<Value>>| {
                        image.clone().map(|mut values| {
                            values.resize(width, Value::Null);
                            values
                        })
                    };
                    let event = base + 1 + offset as u64;
                    self.position.pos = event;
                    stage_row_change(
                        &self.targets[op.table].source,
                        op.table,
                        (pad(&op.before), pad(&op.after)),
                        &self.position,
                        event,
                        &mut self.pending,
                        self.maximum_bytes,
                    )?;
                }
            }
            Change::AddColumn(table) => {
                apply_column_change(
                    &mut self.metadata,
                    DATABASE,
                    &mut self.targets,
                    *table,
                    &mut self.blocked,
                    &statement,
                    // The live source, as the refreshed probe would read it.
                    self.sources[*table].clone(),
                )?;
            }
            Change::Truncate(table) => {
                truncate_target(
                    &mut self.metadata,
                    DATABASE,
                    &mut self.targets[*table],
                    &statement,
                )?;
            }
        }
        self.position.pos = transaction.commit_position();
        commit_pending(
            &mut self.targets,
            &mut self.metadata,
            DATABASE,
            &mut self.position,
            &mut self.pending,
        )?;
        Ok(())
    }

    /// Reopens everything from disk and replays the source past the durable
    /// checkpoint.
    fn restart(&mut self, step: usize) {
        ARMED.with(|armed| armed.set(None));
        self.targets.clear();
        self.pending = PendingTransaction::default();
        self.metadata = MetaStore::open(&self.metadata_path).expect("reopen metadata");
        for source in initial_sources() {
            let directory = self.directory.join(&source.name);
            match CdcTarget::open_tracked(
                &self.metadata_path,
                DATABASE,
                source,
                directory,
                self.options,
            ) {
                Ok(target) => self.targets.push(target),
                Err(error) => {
                    self.fail(step, &format!("reopening a tracked table failed: {error}"))
                }
            }
        }
        let resync = self
            .metadata
            .tables_needing_resync(DATABASE)
            .expect("resync set");
        self.blocked = self
            .targets
            .iter()
            .enumerate()
            .filter(|(_, target)| resync.contains(&target.source.name))
            .map(|(index, _)| index)
            .collect();
        let checkpoint = self
            .metadata
            .snapshot_checkpoint(DATABASE)
            .expect("read checkpoint")
            .unwrap_or_else(|| initial_checkpoint(self.mode));
        let resume = checkpoint.binlog_pos.unwrap_or(0);
        self.position = StreamPosition::from_checkpoint(checkpoint, SourceFlavor::Mysql)
            .expect("resume position");
        let replay = (0..self.log.len())
            .filter(|&index| self.log[index].commit_position() > resume)
            .collect::<Vec<_>>();
        self.trace.push(format!(
            "restart: replay {} transactions past {resume}",
            replay.len()
        ));
        for index in replay {
            if let Err(error) = self.deliver(index) {
                self.fail(
                    step,
                    &format!(
                        "replaying transaction {} failed: {error}",
                        self.log[index].sequence
                    ),
                );
            }
        }
    }

    fn verify(&self, step: usize) {
        for index in 0..self.models.len() {
            self.verify_table(step, index);
        }
        if self.mode == Mode::Gtid
            && let Some(last) = self.log.last()
            && let Some(checkpoint) = self
                .metadata
                .snapshot_checkpoint(DATABASE)
                .expect("checkpoint")
        {
            let wanted = format!(":1-{}", last.sequence);
            let set = checkpoint.gtid_set.unwrap_or_default();
            if last.sequence > 1 && !set.ends_with(&wanted) {
                self.fail(
                    step,
                    &format!(
                        "checkpoint GTID set {set} does not end at {}",
                        last.sequence
                    ),
                );
            }
        }
    }

    fn verify_table(&self, step: usize, index: usize) {
        let model = &self.models[index];
        {
            if self.blocked.contains(&index) {
                self.fail(
                    step,
                    &format!("{} was quarantined", self.sources[index].name),
                );
            }
            let target = &self.targets[index];
            let names = target
                .store
                .schema()
                .columns()
                .iter()
                .map(|c| c.name().to_owned())
                .collect::<Vec<_>>();
            let expected_names = self.sources[index]
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>();
            if names != expected_names {
                self.fail(
                    step,
                    &format!(
                        "{} columns {names:?}, source has {expected_names:?}",
                        target.source.name
                    ),
                );
            }
            let rows = match target.store.snapshot().scan() {
                Ok(rows) => rows,
                Err(error) => self.fail(
                    step,
                    &format!("scan of {} failed: {error}", target.source.name),
                ),
            };
            if model.keyed {
                let actual = rows
                    .iter()
                    .map(|row| (row.key().clone(), row.values().to_vec()))
                    .collect::<BTreeMap<_, _>>();
                if actual != model.rows {
                    let missing = model
                        .rows
                        .iter()
                        .find(|(key, row)| actual.get(*key) != Some(row));
                    let extra = actual
                        .iter()
                        .find(|(key, row)| model.rows.get(*key) != Some(row));
                    self.fail(
                        step,
                        &format!(
                            "{} holds {} rows, source {}; first source row not matched {missing:?}; first stored row not in source {extra:?}",
                            target.source.name,
                            actual.len(),
                            model.rows.len()
                        ),
                    );
                }
            } else {
                let mut actual = rows
                    .iter()
                    .map(|row| format!("{:?}", row.values()))
                    .collect::<Vec<_>>();
                let mut expected = model
                    .appended
                    .iter()
                    .map(|row| format!("{row:?}"))
                    .collect::<Vec<_>>();
                actual.sort();
                expected.sort();
                if actual != expected {
                    self.fail(
                        step,
                        &format!(
                            "{} holds {} appended rows, source {}",
                            target.source.name,
                            actual.len(),
                            expected.len()
                        ),
                    );
                }
            }
        }
    }

    fn run(&mut self, steps: usize) {
        for step in 0..steps {
            let roll = self.rng.below(100);
            let crash = roll >= 92;
            let change = match self.rng.below(100) {
                _ if (72..92).contains(&roll) => None,
                0..=5 => self.generate_add_column(),
                6..=8 => Some(self.generate_truncate()),
                _ => Some(self.generate_rows()),
            };
            if let Some(change) = change {
                self.transaction(step, change, crash);
            } else {
                self.maintenance(step);
            }
            self.verify(step);
        }
    }

    /// Commits one source transaction, optionally dying at a recovery point.
    fn transaction(&mut self, step: usize, change: Change, crash: bool) {
        let sequence = self.log.len() as u64 + 1;
        self.log.push(Transaction { sequence, change });
        let site = crash.then(|| {
            RECOVERY_POINTS
                [usize::try_from(self.rng.below(RECOVERY_POINTS.len() as u64)).expect("site")]
        });
        ARMED.with(|armed| armed.set(site));
        let description = describe(&self.log[self.log.len() - 1]);
        self.trace.push(format!(
            "{description}{}",
            site.map(|s| format!(" crash@{s}")).unwrap_or_default()
        ));
        match self.deliver(self.log.len() - 1) {
            Ok(()) => {
                ARMED.with(|armed| armed.set(None));
                if site.is_some() {
                    self.restart(step);
                }
            }
            Err(error) if error.to_string().contains(CRASH_MARKER) => self.restart(step),
            Err(error) => self.fail(step, &format!("apply failed: {error}")),
        }
    }

    /// Flushes, compacts or reclaims one table, or restarts everything.
    fn maintenance(&mut self, step: usize) {
        let table = usize::try_from(self.rng.below(3)).expect("table");
        let action = self.rng.below(4);
        self.trace.push(format!(
            "maintenance {action} on {}",
            self.sources[table].name
        ));
        if action == 3 {
            self.restart(step);
            return;
        }
        let store = &mut self.targets[table].store;
        let outcome = match action {
            0 => store.flush().map(|_| ()),
            1 => store.compact().map(|_| ()),
            _ => store.reclaim_obsolete_segments().map(|_| ()),
        };
        if let Err(error) = outcome {
            self.fail(step, &format!("maintenance failed: {error}"));
        }
    }
}

fn initial_checkpoint(mode: Mode) -> SnapshotCheckpointRecord {
    SnapshotCheckpointRecord {
        kind: match mode {
            Mode::Gtid => "gtid",
            Mode::FilePosition => "filepos",
        }
        .to_owned(),
        gtid_set: (mode == Mode::Gtid).then(String::new),
        binlog_file: Some(BINLOG_FILE.to_owned()),
        binlog_pos: Some(4),
    }
}

fn describe(transaction: &Transaction) -> String {
    match &transaction.change {
        Change::Rows(ops) => {
            let tables = ops.iter().map(|op| op.table).collect::<BTreeSet<_>>();
            format!(
                "tx {}: {} row changes over tables {tables:?}",
                transaction.sequence,
                ops.len()
            )
        }
        Change::AddColumn(table) => {
            format!("tx {}: add column to table {table}", transaction.sequence)
        }
        Change::Truncate(table) => format!("tx {}: truncate table {table}", transaction.sequence),
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

#[test]
fn change_capture_matches_the_source_through_crashes_restarts_and_schema_changes() {
    let steps = usize::try_from(env_number("PINTAIL_CDC_SIM_STEPS", 120)).expect("steps");
    let seeds = std::env::var("PINTAIL_CDC_SIM_SEED")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .map_or_else(
            || {
                let base = env_number("PINTAIL_CDC_SIM_SEED_BASE", 1);
                (base..base + env_number("PINTAIL_CDC_SIM_SEEDS", 16)).collect()
            },
            |seed| vec![seed],
        );
    for seed in seeds {
        let workspace = tempfile::tempdir().expect("simulation workspace");
        Simulation::new(seed, workspace.path()).run(steps);
    }
}

#[test]
fn a_keyless_table_refuses_updates_and_deletes_instead_of_guessing() {
    let workspace = tempfile::tempdir().expect("simulation workspace");
    let mut simulation = Simulation::new(7, workspace.path());
    let row = vec![Value::Utf8("x".to_owned()), Value::Int64(1)];
    for (before, after) in [(Some(row.clone()), None), (Some(row.clone()), Some(row))] {
        let refusal = stage_row_change(
            &simulation.targets[2].source,
            2,
            (before, after),
            &simulation.position,
            10,
            &mut simulation.pending,
            simulation.maximum_bytes,
        )
        .expect_err("keyless change");
        assert!(
            refusal.to_string().contains("requires resnapshot"),
            "{refusal}"
        );
    }
}
