//! A scan that has to merge, timed against the same rows with nothing to
//! merge. Ignored: a measurement, not a gate. Run with `cargo test
//! --release -p pintail-store --test merge_output -- --ignored
//! --nocapture`.
//!
//! Both stores are built before either is measured, and the two arms
//! alternate round by round. The first version of this measurement built
//! and timed the merging store, then built and timed the direct one, so
//! the two arms met different allocator and page-cache states and the
//! ratio carried whatever that difference was worth. A number this large
//! should not rest on the order the arms happen to run in.
use std::time::Instant;

use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BASE_ROWS: u64 = 2_000_000;
const CHANGED: u64 = 20_000;
const ROUNDS: usize = 5;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, false),
            Column::new(3, "note", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("status-{}", id % 5)),
            Value::Utf8(format!("a note for row {id} that is not tiny")),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        version,
        false,
    )
}

/// One full projected scan, returning its duration and the rows it saw.
fn timed_scan(snapshot: &TableSnapshot) -> (f64, usize) {
    timed_projection(snapshot, &[1, 2, 3, 4])
}

fn timed_projection(snapshot: &TableSnapshot, projection: &[u32]) -> (f64, usize) {
    let low = PrimaryKey::new(vec![KeyPart::UInt64(0)]).expect("low");
    let high = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("high");
    let clock = Instant::now();
    let mut seen = 0;
    if let Some(mut stream) = snapshot
        .scan_projected_range_stream(&low, &high, projection)
        .expect("scan")
    {
        loop {
            let chunks = stream
                .next_column_chunks(projection.len(), 512 << 20)
                .expect("chunks");
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                seen += chunk.row_count();
            }
        }
    }
    (clock.elapsed().as_secs_f64(), seen)
}

#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn a_merging_scan_reads_every_column() {
    // Both stores first, neither measured yet.
    let merged_dir = tempfile::tempdir().expect("directory");
    let mut merged =
        TableStore::open(merged_dir.path(), schema(), StoreOptions::default()).expect("store");
    merged
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    // A scattered update, then a flush, so the scan meets two overlapping
    // segments and has to merge rather than take the overlay.
    let stride = BASE_ROWS / CHANGED;
    merged
        .bulk_ingest_snapshot((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("tail");

    let plain_dir = tempfile::tempdir().expect("directory");
    let mut plain =
        TableStore::open(plain_dir.path(), schema(), StoreOptions::default()).expect("store");
    plain
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");

    let merging = merged.snapshot();
    let direct = plain.snapshot();

    // One warm round of each, discarded, then the arms alternate so a
    // drift in either direction is shared rather than attributed to one.
    let _ = timed_scan(&merging);
    let _ = timed_scan(&direct);
    let mut merging_best = f64::MAX;
    let mut direct_best = f64::MAX;
    for round in 0..ROUNDS {
        // Alternating which arm goes first inside the round as well, so
        // neither is always the one that runs on a colder cache.
        if round % 2 == 0 {
            let (elapsed, seen) = timed_scan(&merging);
            assert_eq!(seen as u64, BASE_ROWS);
            merging_best = merging_best.min(elapsed);
            let (elapsed, seen) = timed_scan(&direct);
            assert_eq!(seen as u64, BASE_ROWS);
            direct_best = direct_best.min(elapsed);
        } else {
            let (elapsed, seen) = timed_scan(&direct);
            assert_eq!(seen as u64, BASE_ROWS);
            direct_best = direct_best.min(elapsed);
            let (elapsed, seen) = timed_scan(&merging);
            assert_eq!(seen as u64, BASE_ROWS);
            merging_best = merging_best.min(elapsed);
        }
    }

    println!(
        "{BASE_ROWS} rows, four columns, {CHANGED} changed ({:.1}%), \
         {ROUNDS} alternating rounds after a discarded warm-up",
        f64::from(u32::try_from(CHANGED).expect("small")) * 100.0
            / f64::from(u32::try_from(BASE_ROWS).expect("small"))
    );
    println!(
        "  merging scan (two segments) = {:8.1} ms",
        merging_best * 1e3
    );
    println!(
        "  direct scan  (one segment)  = {:8.1} ms",
        direct_best * 1e3
    );
    println!(
        "  the merge costs             = {:8.1}x",
        merging_best / direct_best
    );
}

/// What compacting the overlap costs, against what leaving it costs every
/// scan. Ignored: a measurement, not a gate.
///
/// The scan measurement above says a base plus one overlapping tail is a
/// hundredfold slower to read than the same rows in one segment. It does
/// not say whether merging them is the right answer, because merging
/// rewrites the whole base to absorb a tail one percent its size. This
/// times both sides of that trade so the policy can be argued from
/// numbers: the one-off rewrite, and the per-scan penalty it removes.
#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn compacting_the_overlap_against_paying_for_it_per_scan() {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    let stride = BASE_ROWS / CHANGED;
    store
        .bulk_ingest_snapshot((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("tail");

    let overlapping = store.snapshot();
    let _ = timed_scan(&overlapping);
    let mut merging = f64::MAX;
    for _ in 0..3 {
        let (elapsed, seen) = timed_scan(&overlapping);
        assert_eq!(seen as u64, BASE_ROWS);
        merging = merging.min(elapsed);
    }
    drop(overlapping);

    // What the store's own policy decides today, before anything is forced.
    let planned = store.compaction_status().expect("status");
    let clock = Instant::now();
    let outcome = store.compact().expect("compact");
    let compaction = clock.elapsed().as_secs_f64();

    // What the rewrite would cost if the policy did choose it: compaction
    // writes the merged rows into one new segment, which is the same work
    // as building the base was. Measured separately because the policy
    // above declines to do it, and the trade cannot be argued without it.
    let rewrite_dir = tempfile::tempdir().expect("directory");
    let mut rewritten =
        TableStore::open(rewrite_dir.path(), schema(), StoreOptions::default()).expect("store");
    let clock = Instant::now();
    rewritten
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("rewrite");
    let rewrite = clock.elapsed().as_secs_f64();

    let compacted = store.snapshot();
    let _ = timed_scan(&compacted);
    let mut after = f64::MAX;
    for _ in 0..3 {
        let (elapsed, seen) = timed_scan(&compacted);
        assert_eq!(seen as u64, BASE_ROWS);
        after = after.min(elapsed);
    }

    println!();
    println!(
        "{BASE_ROWS} rows, {CHANGED} changed ({:.1}%), one base and one overlapping tail",
        f64::from(u32::try_from(CHANGED).expect("small")) * 100.0
            / f64::from(u32::try_from(BASE_ROWS).expect("small"))
    );
    println!("  what the policy plans now    = {planned:?}");
    println!("  scan while they overlap      = {:8.1} ms", merging * 1e3);
    println!(
        "  compact them, once           = {:8.1} ms  ({outcome:?})",
        compaction * 1e3
    );
    println!("  scan afterwards              = {:8.1} ms", after * 1e3);
    println!(
        "  writing {BASE_ROWS} rows as one segment = {:8.1} ms",
        rewrite * 1e3
    );
    println!(
        "  a scan of one segment        = {:8.1} ms",
        timed_scan(&rewritten.snapshot()).0 * 1e3
    );
    println!(
        "  so a forced rewrite pays for itself after {:.2} scans",
        rewrite / (merging - timed_scan(&rewritten.snapshot()).0)
    );
}

/// What a table pays while it is being written to, which is the state a
/// mirror spends most of its life in: one stamped segment and a memtable
/// holding the rows that changed since the last flush. Ignored: a
/// measurement, not a gate.
///
/// e81 timed two overlapping SEGMENTS, and e91 shortened how long a table
/// stays in that state. Neither says what continuous ingest costs, because
/// changed rows sit in the memtable and the scan takes the overlay path -
/// decoding the segment directly and masking the rows the memtable
/// supersedes - rather than merging two segments. This times that against
/// the same rows with nothing to mask.
#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn an_overlay_scan_against_a_direct_one() {
    let overlay_dir = tempfile::tempdir().expect("directory");
    let mut overlaid =
        TableStore::open(overlay_dir.path(), schema(), StoreOptions::default()).expect("store");
    overlaid
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    // Changed rows stay in the memtable: no flush, so the scan masks them
    // over the segment instead of merging two of them.
    let stride = BASE_ROWS / CHANGED;
    overlaid
        .ingest_cdc((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("live changes");

    let plain_dir = tempfile::tempdir().expect("directory");
    let mut plain =
        TableStore::open(plain_dir.path(), schema(), StoreOptions::default()).expect("store");
    plain
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");

    let overlay = overlaid.snapshot();
    let direct = plain.snapshot();
    let _ = timed_scan(&overlay);
    let _ = timed_scan(&direct);
    let mut overlay_best = f64::MAX;
    let mut direct_best = f64::MAX;
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            overlay_best = overlay_best.min(timed_scan(&overlay).0);
            direct_best = direct_best.min(timed_scan(&direct).0);
        } else {
            direct_best = direct_best.min(timed_scan(&direct).0);
            overlay_best = overlay_best.min(timed_scan(&overlay).0);
        }
    }

    println!();
    println!(
        "{BASE_ROWS} rows, {CHANGED} changed and still in the memtable ({:.1}%)",
        f64::from(u32::try_from(CHANGED).expect("small")) * 100.0
            / f64::from(u32::try_from(BASE_ROWS).expect("small"))
    );
    println!(
        "  overlay scan (segment + memtable) = {:8.1} ms",
        overlay_best * 1e3
    );
    println!(
        "  direct scan  (nothing to mask)    = {:8.1} ms",
        direct_best * 1e3
    );
    println!(
        "  the overlay costs                 = {:8.2}x",
        overlay_best / direct_best
    );
}

/// Which column pays. Ignored: a measurement, not a gate.
#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn what_the_overlay_costs_by_column() {
    let overlay_dir = tempfile::tempdir().expect("directory");
    let mut overlaid =
        TableStore::open(overlay_dir.path(), schema(), StoreOptions::default()).expect("store");
    overlaid
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    let stride = BASE_ROWS / CHANGED;
    overlaid
        .ingest_cdc((0..CHANGED).map(|n| row(n * stride + 1, 2)).collect())
        .expect("live changes");
    let plain_dir = tempfile::tempdir().expect("directory");
    let mut plain =
        TableStore::open(plain_dir.path(), schema(), StoreOptions::default()).expect("store");
    plain
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    let overlay = overlaid.snapshot();
    let direct = plain.snapshot();

    println!();
    println!("{BASE_ROWS} rows, {CHANGED} in the memtable; ms by projection");
    println!(
        "{:>28}  {:>10}  {:>10}  {:>8}",
        "projection", "overlay", "direct", "ratio"
    );
    for (label, projection) in [
        ("id (UInt64 key)", &[1_u32][..]),
        ("status (5 distinct text)", &[2][..]),
        ("note (unique text)", &[3][..]),
        ("amount (Int64)", &[4][..]),
        ("all four", &[1, 2, 3, 4][..]),
    ] {
        let _ = timed_projection(&overlay, projection);
        let _ = timed_projection(&direct, projection);
        let mut o = f64::MAX;
        let mut d = f64::MAX;
        for _ in 0..3 {
            o = o.min(timed_projection(&overlay, projection).0);
            d = d.min(timed_projection(&direct, projection).0);
        }
        println!(
            "{label:>28}  {:>10.1}  {:>10.1}  {:>7.1}x",
            o * 1e3,
            d * 1e3,
            o / d
        );
    }
}

/// How the overlay's cost scales with how many rows changed. Ignored: a
/// measurement, not a gate.
#[test]
#[ignore = "a measurement over a large table, not a gate"]
fn what_the_overlay_costs_by_how_much_changed() {
    let plain_dir = tempfile::tempdir().expect("directory");
    let mut plain =
        TableStore::open(plain_dir.path(), schema(), StoreOptions::default()).expect("store");
    plain
        .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
        .expect("base");
    let direct = plain.snapshot();
    let _ = timed_projection(&direct, &[1]);
    let mut baseline = f64::MAX;
    for _ in 0..3 {
        baseline = baseline.min(timed_projection(&direct, &[1]).0);
    }

    println!();
    println!(
        "{BASE_ROWS} rows, one projected column, direct scan = {:.1} ms",
        baseline * 1e3
    );
    println!("{:>12}  {:>10}  {:>8}", "changed", "overlay ms", "ratio");
    for changed in [1_u64, 10, 100, 1_000, 10_000, 20_000] {
        let dir = tempfile::tempdir().expect("directory");
        let mut store =
            TableStore::open(dir.path(), schema(), StoreOptions::default()).expect("store");
        store
            .bulk_ingest_snapshot((1..=BASE_ROWS).map(|id| row(id, 1)).collect())
            .expect("base");
        let stride = BASE_ROWS / changed;
        store
            .ingest_cdc((0..changed).map(|n| row(n * stride + 1, 2)).collect())
            .expect("live");
        let snapshot = store.snapshot();
        let _ = timed_projection(&snapshot, &[1]);
        let mut best = f64::MAX;
        for _ in 0..3 {
            best = best.min(timed_projection(&snapshot, &[1]).0);
        }
        println!(
            "{changed:>12}  {:>10.1}  {:>7.1}x",
            best * 1e3,
            best / baseline
        );
    }
}
