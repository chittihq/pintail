//! Generated disk faults against a table's files.
//!
//! Each seed builds a table through a random run of versioned writes,
//! tombstones, flushes, compactions and checkpoints, remembering the rows the
//! table held after every acknowledged step. The store is closed and one of
//! its files is damaged the way disks and file systems damage them: a flipped
//! bit, a torn tail, a zeroed page, a file gone. Reopening and reading must
//! then do one of two things - refuse with an error, or answer with the rows
//! of some acknowledged moment. A read that returns anything else is data
//! corruption presented as rows, which is the one outcome a mirror must never
//! produce.
//!
//! `PINTAIL_DISK_FAULT_SEEDS` and `PINTAIL_DISK_FAULT_SEED_BASE` widen a run; `PINTAIL_DISK_FAULT_SEED` replays
//! one seed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rand::{Rng, SeedableRng, rngs::StdRng};

type Rows = BTreeMap<u64, Vec<Value>>;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, true),
            Column::new(3, "amount", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn options(rng: &mut StdRng) -> StoreOptions {
    StoreOptions {
        wal_sync: WalSync::Always,
        memtable_bytes: [4 * 1024, 64 * 1024][rng.random_range(0..2)],
        compaction_fan_in: 2,
        background_compaction: false,
        ..StoreOptions::default()
    }
}

fn row(key: u64, values: Vec<Value>, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
        values,
        version,
        deleted,
    )
}

fn values(rng: &mut StdRng, key: u64) -> Vec<Value> {
    let label = match rng.random_range(0..5) {
        0 => Value::Null,
        1 => Value::Utf8(String::new()),
        n => Value::Utf8(format!("label-{}", key * n)),
    };
    let amount = if rng.random_bool(0.1) {
        Value::Null
    } else {
        Value::Int64(rng.random_range(-10_000..10_000))
    };
    vec![Value::UInt64(key), label, amount]
}

/// Builds the table and returns every acknowledged state, oldest first.
fn build(directory: &Path, rng: &mut StdRng, options: StoreOptions) -> Vec<Rows> {
    let mut store = TableStore::open(directory, schema(), options).expect("open table");
    let mut rows = Rows::new();
    let mut states = vec![rows.clone()];
    let mut version = 0_u64;
    for _ in 0..rng.random_range(8..40) {
        match rng.random_range(0..10) {
            0 => {
                store.flush().expect("flush");
            }
            1 => {
                store.compact().expect("compact");
            }
            2 => {
                store.checkpoint().expect("checkpoint");
            }
            _ => {
                let mut batch = Vec::new();
                for _ in 0..rng.random_range(1..24) {
                    version += 1;
                    let key = rng.random_range(0..40);
                    if rows.contains_key(&key) && rng.random_bool(0.3) {
                        let old = rows.remove(&key).expect("present");
                        batch.push(row(key, old, version, true));
                    } else {
                        let new = values(rng, key);
                        rows.insert(key, new.clone());
                        batch.push(row(key, new, version, false));
                    }
                }
                store.ingest_cdc(batch).expect("ingest");
                states.push(rows.clone());
            }
        }
    }
    if rng.random_bool(0.5) {
        store.flush().expect("final flush");
    }
    store.checkpoint().expect("final checkpoint");
    states
}

fn files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("list table files") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("lock"))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Damages one file and describes what was done.
fn damage(path: &Path, rng: &mut StdRng) -> String {
    let mut bytes = std::fs::read(path).expect("read file");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_owned();
    if bytes.is_empty() {
        std::fs::remove_file(path).expect("remove empty file");
        return format!("removed empty {name}");
    }
    let description = match rng.random_range(0..4) {
        0 => {
            let at = rng.random_range(0..bytes.len());
            let bit = rng.random_range(0..8);
            bytes[at] ^= 1 << bit;
            format!("flipped bit {bit} of byte {at} in {name}")
        }
        1 => {
            let keep = rng.random_range(0..bytes.len());
            bytes.truncate(keep);
            format!("truncated {name} to {keep} of its bytes")
        }
        2 => {
            let start = rng.random_range(0..bytes.len());
            let end = (start + 4096).min(bytes.len());
            bytes[start..end].fill(0);
            format!("zeroed bytes {start}..{end} of {name}")
        }
        _ => {
            std::fs::remove_file(path).expect("remove file");
            return format!("removed {name}");
        }
    };
    std::fs::write(path, bytes).expect("write damaged file");
    description
}

fn read_back(directory: &Path, options: StoreOptions) -> Result<Rows, String> {
    let store = TableStore::open(directory, schema(), options).map_err(|e| e.to_string())?;
    let rows = store.snapshot().scan().map_err(|e| e.to_string())?;
    let mut read = Rows::new();
    for stored in rows {
        let key = match stored.key().parts() {
            [KeyPart::UInt64(key)] => *key,
            other => {
                return Ok(BTreeMap::from([(
                    u64::MAX,
                    vec![Value::Utf8(format!("{other:?}"))],
                )]));
            }
        };
        if read.insert(key, stored.values().to_vec()).is_some() {
            return Ok(BTreeMap::from([(
                u64::MAX,
                vec![Value::Utf8("duplicate key".to_owned())],
            )]));
        }
    }
    Ok(read)
}

fn run_seed(seed: u64) {
    let mut rng = StdRng::seed_from_u64(seed);
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = workspace.path().join("table");
    let options = options(&mut rng);
    let states = build(&directory, &mut rng, options);
    let candidates = files(&directory);
    assert!(
        !candidates.is_empty(),
        "seed {seed}: the table wrote no files"
    );
    let target = &candidates[rng.random_range(0..candidates.len())];
    let fault = damage(target, &mut rng);
    match read_back(&directory, options) {
        Err(_) => {}
        Ok(rows) => {
            let last = states.last().expect("state");
            let differing = rows
                .iter()
                .find(|(key, values)| last.get(key) != Some(values))
                .map(|(key, values)| {
                    format!("key {key}: read {values:?}, wrote {:?}", last.get(key))
                });
            assert!(
                states.contains(&rows),
                "seed {seed}: after {fault}, the table answered {} rows matching no acknowledged \
                 state (final state held {}); first difference from the final state: {differing:?}; \
                 files: {:?}",
                rows.len(),
                last.len(),
                candidates
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_owned())
                    .collect::<Vec<_>>(),
            );
        }
    }
}

fn env_number(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|raw| raw.parse().ok())
}

#[test]
fn a_damaged_table_refuses_or_answers_an_acknowledged_state() {
    if let Some(seed) = env_number("PINTAIL_DISK_FAULT_SEED") {
        run_seed(seed);
        return;
    }
    let base = env_number("PINTAIL_DISK_FAULT_SEED_BASE").unwrap_or(0);
    for seed in base..base + env_number("PINTAIL_DISK_FAULT_SEEDS").unwrap_or(200) {
        run_seed(seed);
    }
}

#[test]
fn an_undamaged_table_reopens_to_its_final_state() {
    for seed in 0..20 {
        let mut rng = StdRng::seed_from_u64(seed);
        let workspace = tempfile::tempdir().expect("workspace");
        let directory = workspace.path().join("table");
        let options = options(&mut rng);
        let states = build(&directory, &mut rng, options);
        assert_eq!(
            read_back(&directory, options).expect("clean reopen"),
            *states.last().expect("state"),
            "seed {seed}"
        );
    }
}
