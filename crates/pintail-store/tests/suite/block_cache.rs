//! Scans decode blocks from memory once the block cache holds them. A held
//! block is a copy of bytes in one immutable file, so no write may ever be
//! answered from it: these cases warm the cache with repeated scans, change
//! the table by every route that replaces or shadows a file, and require
//! the next scan to return exactly what a cold read would.

use std::ops::Bound;

use pintail_store::{
    BlockCacheAccounting, StoreOptions, TableStore, WalSync, block_cache_stats,
    configure_block_cache, shrink_block_cache,
};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// The cache is one per process: these cases take turns, so that the one
/// which reconfigures it does not empty it under another.
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const STATES: [&str; 4] = ["new", "packed", "sent", "closed"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "state", DataType::Utf8, false),
            Column::new(3, "amount", DataType::Int64, true),
            Column::new(4, "note", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}

fn state_of(id: u64, generation: u64) -> &'static str {
    STATES[usize::try_from((id + generation) % 4).expect("small")]
}

fn amount_of(id: u64, generation: u64) -> Value {
    if id.is_multiple_of(11) {
        Value::Null
    } else {
        // Scattered, so its blocks are stored raw.
        let mixed = (id + generation * 1_000_003).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        Value::Int64(i64::try_from(mixed >> 24).expect("fits"))
    }
}

/// Text that is different in every row and half alike: LZ4 shrinks its
/// blocks, but not by much, which is what the cache holds for scans.
fn note_of(id: u64, generation: u64) -> String {
    let mixed = (id + generation * 7_919).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("note for row {id:08}: {mixed:016x}")
}

fn row(id: u64, generation: u64, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![
            Value::UInt64(id),
            Value::Utf8(state_of(id, generation).to_owned()),
            amount_of(id, generation),
            Value::Utf8(note_of(id, generation)),
        ],
        version,
        deleted,
    )
}

fn options() -> StoreOptions {
    StoreOptions {
        wal_sync: WalSync::Off,
        background_compaction: false,
        block_rows: 512,
        ..StoreOptions::default()
    }
}

type Visible = Vec<(u64, String, Option<i64>, String)>;

/// The table through the streaming projected scan, the path that decodes
/// blocks in bulk.
fn projected(table: &TableStore) -> Visible {
    let snapshot = table.snapshot();
    let mut rows = Vec::new();
    let mut push = |values: &[Value]| match values {
        [
            Value::UInt64(id),
            Value::Utf8(state),
            amount,
            Value::Utf8(note),
        ] => rows.push((
            *id,
            state.clone(),
            match amount {
                Value::Int64(amount) => Some(*amount),
                Value::Null => None,
                other => panic!("unexpected amount {other:?}"),
            },
            note.clone(),
        )),
        other => panic!("unexpected projected row {other:?}"),
    };
    let mut stream = snapshot
        .scan_projected_range_stream(&key(u64::MIN), &key(u64::MAX), &[1, 2, 3, 4])
        .expect("stream");
    if let Some(stream) = stream.as_mut() {
        while let Some(chunk) = stream.next_chunk(64 * 1024 * 1024).expect("chunk") {
            for values in chunk.rows() {
                push(values);
            }
        }
    } else {
        let scan = snapshot
            .scan_projected_range(&key(u64::MIN), &key(u64::MAX), &[1, 2, 3, 4])
            .expect("bounded scan");
        for projected in scan.rows() {
            push(projected.values());
        }
    }
    rows.sort_unstable();
    rows
}

/// Scans several times over, each scan free to use what the ones before
/// it left held; returns the last.
fn repeated(table: &TableStore) -> Visible {
    let mut last = projected(table);
    for _ in 0..3 {
        let next = projected(table);
        assert_eq!(next, last, "a repeated scan changed its answer");
        last = next;
    }
    last
}

/// [`repeated`] over segments that do not overlap, whose blocks a scan
/// decodes in bulk: the cache must have answered at least one of them, so
/// what follows runs against held blocks.
fn warm(table: &TableStore) -> Visible {
    let before = block_cache_stats().hits;
    let last = repeated(table);
    assert!(
        block_cache_stats().hits > before,
        "repeated scans of one segment decoded nothing from held blocks"
    );
    last
}

fn expected(ids: impl Iterator<Item = u64>, generation: impl Fn(u64) -> u64) -> Visible {
    ids.map(|id| {
        let generation = generation(id);
        (
            id,
            state_of(id, generation).to_owned(),
            match amount_of(id, generation) {
                Value::Int64(amount) => Some(amount),
                _ => None,
            },
            note_of(id, generation),
        )
    })
    .collect()
}

#[test]
fn held_blocks_answer_repeated_scans_exactly() {
    let _turn = turn();
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest((1..=4_000).map(|id| row(id, 0, 1, false)).collect())
        .expect("seed");
    table.flush().expect("flush");
    assert_eq!(warm(&table), expected(1..=4_000, |_| 0));
    let stats = block_cache_stats();
    assert!(stats.held_bytes > 0 && stats.held_bytes <= stats.limit_bytes);
}

#[test]
fn a_write_over_held_blocks_is_seen_in_the_memtable_and_after_its_flush() {
    let _turn = turn();
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest((1..=4_000).map(|id| row(id, 0, 1, false)).collect())
        .expect("seed");
    table.flush().expect("flush");
    warm(&table);

    // Updates and deletes scattered through every held block.
    let mut changes = (1..=4_000)
        .filter(|id| id % 7 == 0)
        .map(|id| row(id, 1, 2, false))
        .collect::<Vec<_>>();
    changes.extend(
        (1..=4_000)
            .filter(|id| id % 13 == 0)
            .map(|id| row(id, 1, 3, true)),
    );
    table.ingest(changes).expect("changes");
    let after = expected((1..=4_000).filter(|id| id % 13 != 0), |id| {
        u64::from(id % 7 == 0)
    });
    assert_eq!(projected(&table), after, "memtable rows over held blocks");
    table.flush().expect("flush changes");
    assert_eq!(projected(&table), after, "the flushed overlay");
    assert_eq!(repeated(&table), after);
}

#[test]
fn a_compaction_replaces_held_blocks_with_its_output() {
    let _turn = turn();
    let directory = tempfile::tempdir().expect("tempdir");
    let options = StoreOptions {
        compaction_fan_in: 2,
        ..options()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    table
        .ingest((1..=3_000).map(|id| row(id, 0, 1, false)).collect())
        .expect("first");
    table.flush().expect("flush first");
    table
        .ingest((1_500..=4_500).map(|id| row(id, 2, 2, false)).collect())
        .expect("second");
    table.flush().expect("flush second");
    let merged = expected(1..=4_500, |id| if id >= 1_500 { 2 } else { 0 });
    assert_eq!(repeated(&table), merged);

    table.compact().expect("compact");
    assert_eq!(projected(&table), merged, "straight after the merge");
    table.reclaim_obsolete_segments().expect("reclaim");
    assert_eq!(warm(&table), merged, "after the inputs are deleted");

    drop(table);
    let reopened = TableStore::open(directory.path(), schema(), options).expect("reopen");
    assert_eq!(projected(&reopened), merged);
}

#[test]
fn a_recopied_range_is_read_from_the_new_copy() {
    let _turn = turn();
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    let chunk = |generation: u64, ids: std::ops::RangeInclusive<u64>| {
        ids.map(|id| row(id, generation, 0, false))
            .collect::<Vec<_>>()
    };
    table
        .bulk_ingest_snapshot_covering(
            chunk(0, 1..=2_000),
            (Bound::Unbounded, Bound::Included(key(2_000))),
        )
        .expect("first chunk");
    table
        .bulk_ingest_snapshot_covering(
            chunk(0, 2_001..=4_000),
            (Bound::Excluded(key(2_000)), Bound::Unbounded),
        )
        .expect("second chunk");
    assert_eq!(warm(&table), expected(1..=4_000, |_| 0));

    // The source changed before the second chunk is copied again: one row
    // is gone and every other one differs.
    let recopied = chunk(3, 2_001..=4_000)
        .into_iter()
        .filter(|row| row.key() != &key(3_000))
        .collect();
    table
        .bulk_ingest_snapshot_covering(recopied, (Bound::Excluded(key(2_000)), Bound::Unbounded))
        .expect("recopy");
    let after = expected((1..=4_000).filter(|id| *id != 3_000), |id| {
        if id > 2_000 { 3 } else { 0 }
    });
    assert_eq!(projected(&table), after, "straight after the recopy");
    table.reclaim_obsolete_segments().expect("reclaim");
    assert_eq!(warm(&table), after);
}

#[test]
fn a_segment_write_that_died_leaves_nothing_a_scan_can_be_answered_from() {
    let _turn = turn();
    let directory = tempfile::tempdir().expect("tempdir");
    let published = {
        let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
        table
            .ingest((1..=4_000).map(|id| row(id, 0, 1, false)).collect())
            .expect("seed");
        table.flush().expect("flush");
        table.checkpoint().expect("checkpoint");
        warm(&table)
    };
    // A process killed while writing its next segment leaves a partial
    // temporary file, or a whole file the manifest never named. Both carry
    // real block bytes: a copy of the published segment, cut short.
    let segment = std::fs::read_dir(directory.path())
        .expect("list")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "ptseg")
        })
        .expect("published segment");
    let bytes = std::fs::read(&segment).expect("segment bytes");
    std::fs::write(
        directory
            .path()
            .join(".segment-00000000000000000777.ptseg.tmp"),
        &bytes[..bytes.len() / 2],
    )
    .expect("partial write");
    std::fs::write(
        directory.path().join("segment-00000000000000000778.ptseg"),
        &bytes[..bytes.len() - 9],
    )
    .expect("unpublished write");

    let mut reopened = TableStore::open(directory.path(), schema(), options()).expect("reopen");
    assert_eq!(warm(&reopened), published);
    reopened
        .ingest((4_001..=4_500).map(|id| row(id, 1, 2, false)).collect())
        .expect("more");
    reopened.flush().expect("flush more");
    assert_eq!(
        warm(&reopened),
        expected(1..=4_500, |id| u64::from(id > 4_000))
    );
}

static CHARGED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static REFUSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn charge(bytes: usize) -> bool {
    if REFUSE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    CHARGED.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    true
}

fn release(bytes: usize) {
    CHARGED.fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn held_bytes_are_charged_bounded_and_given_back_on_demand() {
    let _turn = turn();
    let charged = || CHARGED.load(std::sync::atomic::Ordering::Relaxed);
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest((1..=20_000).map(|id| row(id, 0, 1, false)).collect())
        .expect("seed");
    table.flush().expect("flush");
    let rows = expected(1..=20_000, |_| 0);
    let accounting = Some(BlockCacheAccounting { charge, release });

    // Every held byte is charged, and what is held stays under the budget.
    configure_block_cache(32 * 1024 * 1024, accounting);
    assert_eq!(charged(), 0);
    assert_eq!(repeated(&table), rows);
    let held = block_cache_stats().held_bytes;
    assert!(held > 0, "nothing was held");
    assert_eq!(charged(), held);

    // A demand for memory is met oldest first, and the scan still answers.
    let freed = shrink_block_cache(held / 2);
    assert!(freed >= held / 2 && freed <= held);
    assert_eq!(block_cache_stats().held_bytes, held - freed);
    assert_eq!(charged(), held - freed);
    assert_eq!(repeated(&table), rows);
    assert_eq!(charged(), block_cache_stats().held_bytes);

    // A budget too small for the table holds part of it and evicts.
    configure_block_cache(64 * 1024, accounting);
    assert_eq!(charged(), 0, "reconfiguring returns every charge");
    assert_eq!(repeated(&table), rows);
    let stats = block_cache_stats();
    assert!(stats.held_bytes <= stats.limit_bytes);
    assert_eq!(charged(), stats.held_bytes);

    // When the charge is refused nothing is held; at zero nothing is asked.
    configure_block_cache(32 * 1024 * 1024, accounting);
    REFUSE.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(repeated(&table), rows);
    assert_eq!(block_cache_stats().held_bytes, 0);
    REFUSE.store(false, std::sync::atomic::Ordering::Relaxed);
    configure_block_cache(0, accounting);
    assert_eq!(repeated(&table), rows);
    assert_eq!((block_cache_stats().held_bytes, charged()), (0, 0));

    configure_block_cache(32 * 1024 * 1024, None);
}
