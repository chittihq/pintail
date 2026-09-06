//! Generated recovery sequences: random interleavings of versioned writes,
//! tombstones, flushes, compactions, schema evolution, at-least-once
//! replay, a process crash and a restart, checked after every crash and at
//! the end against an in-memory model of what the table must hold. A
//! failing sequence is shrunk to a minimal one before it is reported.
//!
//! The existing crash fuzz kills a worker at a random moment of a fixed
//! write loop; this generates the SHAPE of the run as well as the moment,
//! so a defect that needs a particular order of events - an ADD COLUMN
//! between a flush and a crash, a replayed tombstone after a compaction -
//! has a chance of being reached, and when it is the report is the
//! shortest sequence that still reaches it.
//!
//! Reproduce one sequence with `PINTAIL_RECOVERY_SEQUENCE_FILE=<path>`
//! pointing at a file in the one-op-per-line format the report prints.
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rand::{Rng, SeedableRng, rngs::StdRng};

const WORKER_ENV: &str = "PINTAIL_RECOVERY_SEQUENCE_WORKER";
const DIRECTORY_ENV: &str = "PINTAIL_RECOVERY_SEQUENCE_DIRECTORY";
const OPS_ENV: &str = "PINTAIL_RECOVERY_SEQUENCE_OPS";
const ACK_ENV: &str = "PINTAIL_RECOVERY_SEQUENCE_ACK";
const REPRODUCE_ENV: &str = "PINTAIL_RECOVERY_SEQUENCE_FILE";

/// Distinct keys, small enough that updates, deletes and replays collide.
const KEYS: u64 = 12;
/// Sequences per run of the generated test.
const SEQUENCES: u64 = 24;
/// Operations per generated sequence.
const OPS_PER_SEQUENCE: usize = 48;
/// Shrink attempts before the smallest sequence found so far is reported.
const SHRINK_BUDGET: usize = 160;

/// One step of a sequence. Row operations carry the version the source
/// would have assigned (a binlog position analogue); a lower version
/// arriving later must never win.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Insert {
        key: u64,
        version: u64,
        nulls: u8,
    },
    Update {
        key: u64,
        version: u64,
        nulls: u8,
    },
    Delete {
        key: u64,
        version: u64,
    },
    Flush,
    Compact,
    Reclaim,
    Checkpoint,
    AddColumn,
    /// Re-ingests the last `window` row operations before this one, as
    /// the source first delivered them: an at-least-once stream restarting
    /// from an older position.
    Replay {
        window: usize,
    },
    /// The worker process aborts here without unwinding.
    Crash,
}

impl Op {
    fn render(&self) -> String {
        match self {
            Self::Insert {
                key,
                version,
                nulls,
            } => format!("insert {key} {version} {nulls}"),
            Self::Update {
                key,
                version,
                nulls,
            } => format!("update {key} {version} {nulls}"),
            Self::Delete { key, version } => format!("delete {key} {version}"),
            Self::Flush => "flush".to_owned(),
            Self::Compact => "compact".to_owned(),
            Self::Reclaim => "reclaim".to_owned(),
            Self::Checkpoint => "checkpoint".to_owned(),
            Self::AddColumn => "add-column".to_owned(),
            Self::Replay { window } => format!("replay {window}"),
            Self::Crash => "crash".to_owned(),
        }
    }

    fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        let word = words.next().ok_or_else(|| "empty line".to_owned())?;
        let mut number = |what: &str| -> Result<u64, String> {
            words
                .next()
                .ok_or_else(|| format!("{word}: missing {what}"))?
                .parse::<u64>()
                .map_err(|error| format!("{word}: bad {what}: {error}"))
        };
        Ok(match word {
            "insert" => Self::Insert {
                key: number("key")?,
                version: number("version")?,
                nulls: u8::try_from(number("nulls")?).map_err(|_| "nulls > 255".to_owned())?,
            },
            "update" => Self::Update {
                key: number("key")?,
                version: number("version")?,
                nulls: u8::try_from(number("nulls")?).map_err(|_| "nulls > 255".to_owned())?,
            },
            "delete" => Self::Delete {
                key: number("key")?,
                version: number("version")?,
            },
            "flush" => Self::Flush,
            "compact" => Self::Compact,
            "reclaim" => Self::Reclaim,
            "checkpoint" => Self::Checkpoint,
            "add-column" => Self::AddColumn,
            "replay" => Self::Replay {
                window: usize::try_from(number("window")?).map_err(|_| "window".to_owned())?,
            },
            "crash" => Self::Crash,
            other => return Err(format!("unknown op {other}")),
        })
    }
}

fn render_sequence(ops: &[Op]) -> String {
    let mut text = String::new();
    for op in ops {
        let _ = writeln!(text, "{}", op.render());
    }
    text
}

fn parse_sequence(text: &str) -> Result<Vec<Op>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(Op::parse)
        .collect()
}

fn generate(seed: u64) -> Vec<Op> {
    let mut random = StdRng::seed_from_u64(seed);
    let mut ops = Vec::with_capacity(OPS_PER_SEQUENCE);
    let mut version = 0_u64;
    let mut row_ops = 0_usize;
    let crash_at = random.random_range(1..OPS_PER_SEQUENCE - 1);
    for index in 0..OPS_PER_SEQUENCE {
        if index == crash_at {
            ops.push(Op::Crash);
            continue;
        }
        let roll = random.random_range(0..100);
        let key = random.random_range(1..=KEYS);
        let op = match roll {
            0..=24 => {
                version += random.random_range(1..3);
                row_ops += 1;
                Op::Insert {
                    key,
                    version,
                    nulls: random.random_range(0..4),
                }
            }
            25..=39 => {
                version += random.random_range(1..3);
                row_ops += 1;
                Op::Update {
                    key,
                    version,
                    nulls: random.random_range(0..4),
                }
            }
            40..=52 => {
                version += random.random_range(1..3);
                row_ops += 1;
                Op::Delete { key, version }
            }
            53..=66 => Op::Flush,
            67..=74 => Op::Compact,
            75..=79 => Op::Reclaim,
            80..=84 => Op::Checkpoint,
            85..=90 => Op::AddColumn,
            _ if row_ops > 0 => Op::Replay {
                window: random.random_range(1..=row_ops.min(6)),
            },
            _ => Op::Flush,
        };
        ops.push(op);
    }
    ops
}

// ---------------------------------------------------------------------------
// The model: what the table must hold after a prefix of the sequence.

fn base_columns() -> Vec<Column> {
    vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "name", DataType::Utf8, false),
    ]
}

/// One row as the source delivered it: key, version, tombstone flag and
/// the values by column id under the schema of that moment.
type Delivery = (u64, u64, bool, Vec<(u32, Value)>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelRow {
    version: u64,
    deleted: bool,
    /// Values by column id, as first written; columns added later read NULL.
    values: Vec<(u32, Value)>,
}

#[derive(Clone, Debug)]
struct Model {
    schema_version: u32,
    columns: Vec<Column>,
    rows: BTreeMap<u64, ModelRow>,
    /// Every row operation applied so far, as the source delivered it, for
    /// replay: the row's values under the schema of that moment.
    delivered: Vec<Delivery>,
}

impl Model {
    fn new() -> Self {
        Self {
            schema_version: 1,
            columns: base_columns(),
            rows: BTreeMap::new(),
            delivered: Vec::new(),
        }
    }

    fn schema(&self) -> TableSchema {
        TableSchema::new(self.schema_version, self.columns.clone()).expect("schema")
    }

    /// Values a fresh delivery of `op` carries under the current schema.
    fn delivery(&self, op: &Op) -> Option<Delivery> {
        let (key, version, deleted, nulls) = match *op {
            Op::Insert {
                key,
                version,
                nulls,
            }
            | Op::Update {
                key,
                version,
                nulls,
            } => (key, version, false, nulls),
            Op::Delete { key, version } => (key, version, true, 0),
            _ => return None,
        };
        let values = self
            .columns
            .iter()
            .map(|column| {
                let value = match column.id() {
                    1 => Value::UInt64(key),
                    2 => Value::Utf8(if deleted {
                        "gone".to_owned()
                    } else {
                        format!("k{key}-v{version}")
                    }),
                    id => {
                        let slot = id - 3;
                        if deleted || (slot < 8 && nulls & (1 << slot) != 0) {
                            Value::Null
                        } else {
                            Value::Int64(
                                i64::try_from(version).expect("small") * 100 + i64::from(id),
                            )
                        }
                    }
                };
                (column.id(), value)
            })
            .collect();
        Some((key, version, deleted, values))
    }

    /// Applies one delivery with the store's rule: the highest version
    /// wins, an equal or lower one changes nothing.
    fn apply_delivery(&mut self, delivery: &Delivery) {
        let (key, version, deleted, values) = delivery;
        let replace = self
            .rows
            .get(key)
            .is_none_or(|existing| *version > existing.version);
        if replace {
            self.rows.insert(
                *key,
                ModelRow {
                    version: *version,
                    deleted: *deleted,
                    values: values.clone(),
                },
            );
        }
    }

    /// Deliveries a replay op re-sends: the last `window` row operations
    /// as they were first delivered.
    fn replayed(&self, window: usize) -> Vec<Delivery> {
        let start = self.delivered.len().saturating_sub(window);
        self.delivered[start..].to_vec()
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::AddColumn => {
                let id = u32::try_from(self.columns.len() + 1).expect("small");
                self.columns.push(Column::new(
                    id,
                    format!("extra_{id}"),
                    DataType::Int64,
                    true,
                ));
                self.schema_version += 1;
            }
            Op::Replay { window } => {
                for delivery in self.replayed(*window) {
                    self.apply_delivery(&delivery);
                }
            }
            _ => {
                if let Some(delivery) = self.delivery(op) {
                    self.apply_delivery(&delivery);
                    self.delivered.push(delivery);
                }
            }
        }
    }

    /// The visible rows under the current schema: `(key, version, values)`.
    fn expected(&self) -> Vec<(u64, u64, Vec<Value>)> {
        self.rows
            .iter()
            .filter(|(_, row)| !row.deleted)
            .map(|(key, row)| {
                let values = self
                    .columns
                    .iter()
                    .map(|column| {
                        row.values
                            .iter()
                            .find(|(id, _)| *id == column.id())
                            .map_or(Value::Null, |(_, value)| value.clone())
                    })
                    .collect();
                (*key, row.version, values)
            })
            .collect()
    }

    /// The model after the first `count` ops of `ops`.
    fn after(ops: &[Op], count: usize) -> Self {
        let mut model = Self::new();
        for op in &ops[..count.min(ops.len())] {
            model.apply(op);
        }
        model
    }
}

// ---------------------------------------------------------------------------
// Applying an op to a real store.

fn options() -> StoreOptions {
    StoreOptions {
        // Every acknowledged op is durable, so a crash after op k must
        // recover exactly the model after op k.
        wal_sync: WalSync::Always,
        // A tiny memtable makes ingest flush, compact and reclaim on its
        // own, so those paths run between the explicit ops as well.
        memtable_bytes: 4 * 1024,
        compaction_fan_in: 2,
        background_compaction: false,
        ..StoreOptions::default()
    }
}

fn stored_row(delivery: &Delivery, columns: &[Column]) -> StoredRow {
    let (key, version, deleted, values) = delivery;
    let adapted = columns
        .iter()
        .map(|column| {
            values
                .iter()
                .find(|(id, _)| *id == column.id())
                .map_or(Value::Null, |(_, value)| value.clone())
        })
        .collect();
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(*key)]).expect("key"),
        adapted,
        *version,
        *deleted,
    )
}

/// Applies `op` to the store and advances the model in step. `Crash` is
/// the caller's to interpret.
fn apply_to_store(store: &mut TableStore, model: &mut Model, op: &Op) -> Result<(), String> {
    let describe = |what: &str, error: pintail_store::StoreError| format!("{what}: {error}");
    match op {
        Op::Flush => {
            store.flush().map_err(|error| describe("flush", error))?;
        }
        Op::Compact => {
            store
                .compact()
                .map_err(|error| describe("compact", error))?;
        }
        Op::Reclaim => {
            store
                .reclaim_obsolete_segments()
                .map_err(|error| describe("reclaim", error))?;
        }
        Op::Checkpoint => {
            store
                .checkpoint()
                .map_err(|error| describe("checkpoint", error))?;
        }
        Op::AddColumn => {
            model.apply(op);
            store
                .evolve_schema(model.schema())
                .map_err(|error| describe("evolve schema", error))?;
        }
        Op::Replay { window } => {
            let rows = model
                .replayed(*window)
                .iter()
                .map(|delivery| stored_row(delivery, &model.columns))
                .collect::<Vec<_>>();
            if !rows.is_empty() {
                store
                    .ingest_cdc(rows)
                    .map_err(|error| describe("replay ingest", error))?;
            }
            model.apply(op);
        }
        Op::Crash => {}
        Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } => {
            let delivery = model.delivery(op).expect("row op");
            store
                .ingest_cdc(vec![stored_row(&delivery, &model.columns)])
                .map_err(|error| describe("ingest", error))?;
            model.apply(op);
        }
    }
    Ok(())
}

fn actual_rows(store: &TableStore) -> Result<Vec<(u64, u64, Vec<Value>)>, String> {
    let mut rows = store
        .snapshot()
        .scan()
        .map_err(|error| format!("scan: {error}"))?
        .into_iter()
        .map(|row| {
            let [KeyPart::UInt64(key)] = row.key().parts() else {
                panic!("single UInt64 key");
            };
            (*key, row.version(), row.values().to_vec())
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|(key, _, _)| *key);
    Ok(rows)
}

fn check(store: &TableStore, model: &Model, moment: &str) -> Result<(), String> {
    let actual = actual_rows(store)?;
    let expected = model.expected();
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "{moment}: table differs from the model\n  expected: {expected:?}\n  actual:   {actual:?}"
    ))
}

// ---------------------------------------------------------------------------
// The worker process: runs ops until it crashes or runs out.

#[test]
#[ignore = "spawned by the recovery-sequence driver"]
fn recovery_sequence_worker() {
    if std::env::var_os(WORKER_ENV).is_none() {
        return;
    }
    let directory = PathBuf::from(std::env::var_os(DIRECTORY_ENV).expect("directory"));
    let ops = parse_sequence(
        &std::fs::read_to_string(std::env::var_os(OPS_ENV).expect("ops file")).expect("read ops"),
    )
    .expect("parse ops");
    let mut ack = std::fs::File::create(std::env::var_os(ACK_ENV).expect("ack file")).expect("ack");
    let mut model = Model::new();
    let mut store = TableStore::open(&directory, model.schema(), options()).expect("open");
    for (index, op) in ops.iter().enumerate() {
        if *op == Op::Crash {
            // No unwinding, no destructors, no flush of anything buffered:
            // the process is gone as a kill -9 would leave it.
            std::process::abort();
        }
        if let Err(error) = apply_to_store(&mut store, &mut model, op) {
            eprintln!("worker failed at op {index} ({}): {error}", op.render());
            std::process::exit(3);
        }
        writeln!(ack, "{index}").expect("write ack");
        ack.flush().expect("flush ack");
    }
    drop(store);
}

// ---------------------------------------------------------------------------
// The driver: one sequence end to end.

fn last_acknowledged(path: &Path) -> Option<usize> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .filter_map(|line| line.trim().parse::<usize>().ok())
        .next_back()
}

/// Reopens the table with the schema the model holds after `count` ops,
/// falling back to the schema one op later: a crash inside an ADD COLUMN
/// may have published the new generation before the op was acknowledged.
fn reopen(directory: &Path, ops: &[Op], count: usize) -> Result<(TableStore, usize), String> {
    let model = Model::after(ops, count);
    match TableStore::open(directory, model.schema(), options()) {
        Ok(store) => Ok((store, count)),
        Err(first) => {
            if ops.get(count) == Some(&Op::AddColumn) {
                let later = Model::after(ops, count + 1);
                if let Ok(store) = TableStore::open(directory, later.schema(), options()) {
                    return Ok((store, count + 1));
                }
            }
            Err(format!("reopen after op {count}: {first}"))
        }
    }
}

fn run_sequence(ops: &[Op]) -> Result<(), String> {
    run_sequence_with(ops, None).map(|_| ())
}

/// Runs one sequence; `fault` is a `PINTAIL_FAILPOINT` value for the
/// worker (a `failpoints` build only), so the crash strikes INSIDE a WAL
/// write rather than between two ops. Returns whether the worker died by
/// a signal (the abort), so a caller can tell a fault that fired from one
/// whose hit count the sequence never reached.
fn run_sequence_with(ops: &[Op], fault: Option<&str>) -> Result<bool, String> {
    let workspace = tempfile::tempdir().map_err(|error| error.to_string())?;
    let directory = workspace.path().join("table");
    let ops_path = workspace.path().join("ops.txt");
    let ack_path = workspace.path().join("ack.txt");
    std::fs::write(&ops_path, render_sequence(ops)).map_err(|error| error.to_string())?;

    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    if let Some(fault) = fault {
        command.env("PINTAIL_FAILPOINT", fault);
    }
    let status = command
        .args([
            "--ignored",
            "--exact",
            "recovery_sequence_worker",
            "--test-threads=1",
        ])
        .env(WORKER_ENV, "1")
        .env(DIRECTORY_ENV, &directory)
        .env(OPS_ENV, &ops_path)
        .env(ACK_ENV, &ack_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("spawn worker: {error}"))?;
    let crashed = ops.contains(&Op::Crash) || fault.is_some();
    let aborted = !status.status.success() && status.status.code().is_none();
    if !crashed && !status.status.success() {
        return Err(format!(
            "worker failed without a crash op: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    if crashed && status.status.code() == Some(3) {
        return Err(format!(
            "worker failed before its crash: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }

    // Every op the worker acknowledged is durable (WalSync::Always), and
    // nothing past the crash op ran, so the table must hold exactly the
    // model after the last acknowledged op; an op that was in flight when
    // a fault struck may also have landed whole.
    let acknowledged = last_acknowledged(&ack_path).map_or(0, |index| index + 1);
    let (mut store, mut applied) = reopen(&directory, ops, acknowledged)?;
    let mut model = Model::after(ops, applied);
    if let Err(error) = check(&store, &model, "recovered after the crash") {
        let in_flight = Model::after(ops, applied + 1);
        if applied < ops.len() && ops[applied] != Op::Crash && check(&store, &in_flight, "").is_ok()
        {
            model = in_flight;
            applied += 1;
        } else {
            return Err(error);
        }
    }

    // A stream restarting after the crash re-sends what it already sent:
    // the at-least-once tail must not move the table.
    let replay = model
        .replayed(4)
        .iter()
        .map(|delivery| stored_row(delivery, &model.columns))
        .collect::<Vec<_>>();
    if !replay.is_empty() {
        store
            .ingest_cdc(replay)
            .map_err(|error| format!("replay after restart: {error}"))?;
        check(&store, &model, "after replaying the tail post-restart")?;
    }

    // The restarted process carries on with the rest of the sequence.
    for (index, op) in ops.iter().enumerate().skip(applied) {
        if *op == Op::Crash {
            continue;
        }
        apply_to_store(&mut store, &mut model, op)
            .map_err(|error| format!("after restart, op {index} ({}): {error}", op.render()))?;
    }
    check(&store, &model, "at the end of the sequence")?;
    drop(store);

    // A clean close and reopen must show the same table again.
    let store = TableStore::open(&directory, model.schema(), options())
        .map_err(|error| format!("final reopen: {error}"))?;
    check(&store, &model, "after a clean reopen")?;
    Ok(aborted)
}

// ---------------------------------------------------------------------------
// Shrinking: the smallest sub-sequence that still fails.

fn shrink(ops: &[Op], fault: Option<&str>) -> (Vec<Op>, String) {
    let mut current = ops.to_vec();
    let mut failure = run_sequence_with(&current, fault).expect_err("the sequence fails");
    let mut attempts = 0;
    let mut pieces = 2;
    while current.len() > 1 && attempts < SHRINK_BUDGET {
        let piece = current.len().div_ceil(pieces).max(1);
        let mut reduced = false;
        let mut start = 0;
        while start < current.len() && attempts < SHRINK_BUDGET {
            let end = (start + piece).min(current.len());
            let candidate: Vec<Op> = current[..start]
                .iter()
                .chain(&current[end..])
                .cloned()
                .collect();
            // Dropping the crash changes what is being tested; keep it.
            if candidate.is_empty()
                || candidate.iter().filter(|op| **op == Op::Crash).count()
                    != current.iter().filter(|op| **op == Op::Crash).count()
            {
                start = end;
                continue;
            }
            attempts += 1;
            if let Err(error) = run_sequence_with(&candidate, fault) {
                current = candidate;
                failure = error;
                reduced = true;
                pieces = pieces.saturating_sub(1).max(2);
                break;
            }
            start = end;
        }
        if !reduced {
            if piece == 1 {
                break;
            }
            pieces = (pieces * 2).min(current.len());
        }
    }
    (current, failure)
}

fn report(seed: Option<u64>, ops: &[Op], fault: Option<&str>, failure: &str) -> String {
    let (minimal, minimal_failure) = shrink(ops, fault);
    format!(
        "recovery sequence{} failed: {failure}\n\
         shrunk to {} ops (from {}), failing with: {minimal_failure}\n\
         --- minimal sequence (save as a file and set {REPRODUCE_ENV}) ---\n{}---",
        match (seed, fault) {
            (Some(seed), Some(fault)) => format!(" (seed {seed}, fault {fault})"),
            (Some(seed), None) => format!(" (seed {seed})"),
            (None, Some(fault)) => format!(" (fault {fault})"),
            (None, None) => String::new(),
        },
        minimal.len(),
        ops.len(),
        render_sequence(&minimal)
    )
}

#[test]
fn generated_sequences_of_writes_ddl_replay_and_crashes_recover_exactly() {
    if let Some(path) = std::env::var_os(REPRODUCE_ENV) {
        let ops = parse_sequence(&std::fs::read_to_string(path).expect("read sequence"))
            .expect("parse sequence");
        if let Err(failure) = run_sequence(&ops) {
            panic!("{}", report(None, &ops, None, &failure));
        }
        return;
    }
    let base = std::env::var("PINTAIL_RECOVERY_SEQUENCE_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok())
        .unwrap_or(0x5eed_0000);
    for seed in base..base + SEQUENCES {
        let ops = generate(seed);
        if let Err(failure) = run_sequence(&ops) {
            panic!("{}", report(Some(seed), &ops, None, &failure));
        }
    }
}

/// The same sequences with the crash INSIDE a WAL write: a `failpoints`
/// build aborts the worker at the n-th append or sync, so the op in
/// flight may have landed whole or not at all, and the recovered table
/// must equal one of those two models.
#[cfg(feature = "failpoints")]
#[test]
fn generated_sequences_recover_from_a_fault_inside_a_wal_write() {
    const SITES: [&str; 3] = [
        "store.wal.append",
        "store.wal.before_sync",
        "store.wal.sync",
    ];
    let base = std::env::var("PINTAIL_RECOVERY_SEQUENCE_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok())
        .unwrap_or(0x5eed_1000);
    let mut fired = 0;
    for seed in base..base + SEQUENCES {
        let ops: Vec<Op> = generate(seed)
            .into_iter()
            .filter(|op| *op != Op::Crash)
            .collect();
        let writes = ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Replay { .. }
                )
            })
            .count()
            .max(1);
        let mut random = StdRng::seed_from_u64(seed ^ 0xfa17);
        let site = SITES[random.random_range(0..SITES.len())];
        let hit = random.random_range(1..=writes);
        let fault = format!("{site}@{hit}=abort");
        match run_sequence_with(&ops, Some(&fault)) {
            Ok(true) => fired += 1,
            Ok(false) => {}
            Err(failure) => panic!("{}", report(Some(seed), &ops, Some(&fault), &failure)),
        }
    }
    assert!(
        fired > SEQUENCES / 4,
        "the fault must strike inside a WAL write in a fair share of sequences, fired {fired} of {SEQUENCES}"
    );
}

/// The generator's own contract: every op kind appears across the seeds,
/// the model applies the version rule, and the text form round-trips.
#[test]
fn generated_sequences_cover_every_op_kind_and_round_trip() {
    let mut seen = [false; 10];
    for seed in 0..64 {
        let ops = generate(seed);
        assert_eq!(ops.iter().filter(|op| **op == Op::Crash).count(), 1);
        assert_eq!(parse_sequence(&render_sequence(&ops)).expect("parse"), ops);
        for op in &ops {
            let slot = match op {
                Op::Insert { .. } => 0,
                Op::Update { .. } => 1,
                Op::Delete { .. } => 2,
                Op::Flush => 3,
                Op::Compact => 4,
                Op::Reclaim => 5,
                Op::Checkpoint => 6,
                Op::AddColumn => 7,
                Op::Replay { .. } => 8,
                Op::Crash => 9,
            };
            seen[slot] = true;
        }
    }
    assert!(seen.iter().all(|seen| *seen), "every op kind: {seen:?}");

    let mut model = Model::new();
    model.apply(&Op::Insert {
        key: 1,
        version: 5,
        nulls: 0,
    });
    model.apply(&Op::AddColumn);
    model.apply(&Op::Update {
        key: 1,
        version: 3,
        nulls: 0,
    });
    let expected = model.expected();
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].1, 5, "a lower version never wins");
    assert_eq!(
        expected[0].2,
        vec![
            Value::UInt64(1),
            Value::Utf8("k1-v5".to_owned()),
            Value::Null
        ],
        "a column added later reads NULL for an earlier row"
    );
    model.apply(&Op::Replay { window: 2 });
    assert_eq!(model.expected(), expected, "replay changes nothing");
    model.apply(&Op::Delete { key: 1, version: 9 });
    assert!(model.expected().is_empty());
}
