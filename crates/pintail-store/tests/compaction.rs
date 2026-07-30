use std::collections::BTreeMap;

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use rand::{Rng, SeedableRng, rngs::StdRng};

#[test]
fn full_size_tier_compaction_collapses_versions_and_reclaims_after_snapshots_release() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        compaction_fan_in: 4,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    for batch in [
        vec![row(1, "old", 1, false), row(2, "remove", 1, false)],
        vec![row(1, "new", 3, false), row(3, "remove", 1, false)],
        vec![row(1, "stale", 2, false), row(2, "gone", 4, true)],
        vec![row(3, "gone", 2, true), row(4, "keep", 1, false)],
    ] {
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }

    let expected = vec![row(1, "new", 3, false), row(4, "keep", 1, false)];
    let pinned = table.snapshot();
    assert_eq!(pinned.scan().expect("pre-compaction scan"), expected);
    let status = table.compaction_status().expect("compaction status");
    assert_eq!(status.eligible_segments(), 4);
    assert!(status.debt_bytes() > 0);

    let outcome = table.compact().expect("compact");
    assert_eq!(outcome.input_segments(), 4);
    assert_eq!(outcome.output_rows(), 2);
    assert_eq!(table.snapshot().scan().expect("compacted scan"), expected);
    assert_eq!(
        outcome
            .output_path()
            .map(std::fs::read)
            .transpose()
            .expect("compacted segment")
            .map(|bytes| block_compressions(&bytes)),
        Some(vec![2]),
        "a full merge writes the coldest tier with zstd"
    );

    assert_eq!(
        table.reclaim_obsolete_segments().expect("pinned reclaim"),
        0
    );
    drop(pinned);
    assert_eq!(
        table.reclaim_obsolete_segments().expect("released reclaim"),
        4
    );
    assert_eq!(segment_count(directory.path()), 1);
    drop(table);

    let reopened = TableStore::open(directory.path(), schema(), options).expect("reopen");
    assert_eq!(reopened.snapshot().scan().expect("reopen scan"), expected);
}

#[test]
fn partial_compaction_retains_tombstones_that_suppress_unmerged_versions() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        compaction_fan_in: 2,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    for batch in [
        vec![row(1, "aaaa", 1, false)],
        vec![row(1, "bbbb", 3, true)],
        vec![row(1, "cccc", 0, false)],
    ] {
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }
    assert!(table.snapshot().scan().expect("scan").is_empty());

    let outcome = table.compact().expect("partial compact");
    assert_eq!(outcome.input_segments(), 2);
    assert_eq!(outcome.output_rows(), 1, "partial merge retains tombstone");
    assert!(table.snapshot().scan().expect("partial scan").is_empty());

    let outcome = table.compact().expect("full compact");
    assert_eq!(outcome.input_segments(), 2);
    assert_eq!(outcome.output_rows(), 0, "full merge drops tombstone");
    assert!(table.snapshot().scan().expect("full scan").is_empty());
}

#[test]
fn compaction_defers_a_candidate_above_the_configured_row_bound() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        compaction_fan_in: 2,
        max_compaction_rows: 3,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    for batch in [
        vec![row(1, "old", 1, false), row(2, "first", 1, false)],
        vec![row(1, "new", 2, false), row(3, "second", 1, false)],
    ] {
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }

    let status = table.compaction_status().expect("compaction status");
    assert_eq!(status.segment_count(), 2);
    assert_eq!(status.eligible_segments(), 0);
    assert_eq!(status.debt_bytes(), 0);
    assert_eq!(
        table.compact().expect("bounded compact").input_segments(),
        0
    );
    assert_eq!(
        table.snapshot().scan().expect("scan deferred segments"),
        vec![
            row(1, "new", 2, false),
            row(2, "first", 1, false),
            row(3, "second", 1, false),
        ]
    );
}

#[test]
fn pinned_snapshot_segments_survive_writer_drop_and_reopen() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        compaction_fan_in: 2,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    for batch in [vec![row(1, "old", 1, false)], vec![row(1, "new", 2, false)]] {
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }
    let pinned = table.snapshot();
    table.compact().expect("compact");
    assert_eq!(segment_count(directory.path()), 3);
    drop(table);

    let reopened = TableStore::open(directory.path(), schema(), options).expect("reopen");
    assert_eq!(
        pinned.scan().expect("scan snapshot across writer reopen"),
        vec![row(1, "new", 2, false)]
    );
    assert_eq!(
        segment_count(directory.path()),
        3,
        "reopen preserves segments owned by a live snapshot"
    );
    drop(pinned);
    drop(reopened);

    let _reopened = TableStore::open(directory.path(), schema(), options).expect("reopen again");
    assert_eq!(
        segment_count(directory.path()),
        1,
        "a later reopen removes files after the snapshot releases"
    );
}

#[cfg(unix)]
#[test]
fn snapshot_pins_use_canonical_identity_across_symlinked_reopen() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("temporary parent directory");
    let physical = parent.path().join("physical-table");
    let alias = parent.path().join("table-alias");
    std::fs::create_dir(&physical).expect("physical table directory");
    symlink(&physical, &alias).expect("table symlink");
    let options = StoreOptions {
        compaction_fan_in: 2,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(&physical, schema(), options).expect("open");
    for batch in [vec![row(1, "old", 1, false)], vec![row(1, "new", 2, false)]] {
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }
    let pinned = table.snapshot();
    table.compact().expect("compact");
    drop(table);

    let _reopened = TableStore::open(&alias, schema(), options).expect("reopen through symlink");
    assert_eq!(
        pinned.scan().expect("scan pinned symlink identity"),
        vec![row(1, "new", 2, false)]
    );
    assert_eq!(
        segment_count(&physical),
        3,
        "canonical snapshot identity protects retired files"
    );
}

#[test]
fn arbitrary_version_and_tombstone_interleavings_match_a_naive_reference() {
    for seed in 0..96 {
        check_random_compaction(seed);
    }
}

fn check_random_compaction(seed: u64) {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let options = StoreOptions {
        compaction_fan_in: 4,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    let mut random = StdRng::seed_from_u64(seed);
    let mut reference = BTreeMap::new();
    let segment_count = random.random_range(4..=8);
    let mut event = 0_u64;
    for _ in 0..segment_count {
        let mut batch = Vec::new();
        for _ in 0..random.random_range(8..=40) {
            event += 1;
            let id = random.random_range(0..16);
            let deleted = random.random_ratio(1, 5);
            let version = random.random_range(0..=128) * 1_000 + event;
            let value = row(id, &format!("event-{event}-v{version}"), version, deleted);
            if reference
                .get(&id)
                .is_none_or(|current: &StoredRow| value.version() >= current.version())
            {
                reference.insert(id, value.clone());
            }
            batch.push(value);
        }
        table.ingest(batch).expect("random ingest");
        table.flush().expect("random flush");
    }
    let expected = reference
        .into_values()
        .filter(|row| !row.is_deleted())
        .collect::<Vec<_>>();
    assert_eq!(
        table.snapshot().scan().expect("before compact"),
        expected,
        "seed {seed} before compaction"
    );

    for pass in 0..16 {
        let outcome = table.compact().expect("random compact");
        if outcome.input_segments() == 0 {
            break;
        }
        assert_eq!(
            table.snapshot().scan().expect("after compact"),
            expected,
            "seed {seed} after compaction pass {pass}"
        );
    }
    drop(table);
    let reopened = TableStore::open(directory.path(), schema(), options).expect("random reopen");
    assert_eq!(
        reopened.snapshot().scan().expect("after reopen"),
        expected,
        "seed {seed} after reopen"
    );
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, label: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(label.into())],
        version,
        deleted,
    )
}

fn segment_count(directory: &std::path::Path) -> usize {
    std::fs::read_dir(directory)
        .expect("table directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "ptseg")
        })
        .count()
}

fn block_compressions(bytes: &[u8]) -> Vec<u8> {
    let column_count = read_u32(bytes, 26) as usize;
    let mut position = 34;
    let mut compressions = Vec::new();
    for _ in 0..column_count {
        position += 5;
        let block_count = take_u32(bytes, &mut position);
        for _ in 0..block_count {
            let block_length = take_u32(bytes, &mut position) as usize;
            let block = &bytes[position..position + block_length];
            let mut block_position = 4;
            skip_bytes(block, &mut block_position);
            block_position += 1;
            compressions.push(block[block_position]);
            position += block_length;
            position += 8;
        }
    }
    compressions.sort_unstable();
    compressions.dedup();
    compressions
}

fn skip_bytes(bytes: &[u8], position: &mut usize) {
    let length = take_u32(bytes, position) as usize;
    *position += length;
}

fn take_u32(bytes: &[u8], position: &mut usize) -> u32 {
    let value = read_u32(bytes, *position);
    *position += 4;
    value
}

fn read_u32(bytes: &[u8], position: usize) -> u32 {
    u32::from_le_bytes(bytes[position..position + 4].try_into().expect("u32"))
}
