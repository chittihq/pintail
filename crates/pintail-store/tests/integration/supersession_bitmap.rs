//! Whether the overlay's superseded-row mask should be recomputed by every
//! scan or maintained as rows arrive. Ignored: a measurement, not a gate.
//! Run with `cargo test --release -p pintail-store --test
//! supersession_bitmap -- --ignored --nocapture`.
//!
//! Both arms run over two shapes of change. Evenly scattered keys touch
//! every part of the key column and no lookup reuses a cache line the
//! last one warmed; clustered keys, which is what a burst of recent
//! activity leaves, reuse the same lines repeatedly. The first version of
//! this measurement had only the scattered shape, and a per-change cost
//! read from it is the pessimistic end rather than the range.
//!
//! Not varied, deliberately: how dense the key VALUES are. The keys are
//! one contiguous array whatever they hold, and the mask is indexed by row
//! position, so sparse values change neither the search nor the bitmap.
use std::time::Instant;

const SEGMENT_ROWS: usize = 10_000_000;
const BLOCK_ROWS: usize = 8_192;

/// The change-driven mask of e79: cheap, but every scan pays it again.
fn build_mask(keys: &[i64], changed: &[i64]) -> Vec<u64> {
    let mut bits = vec![0_u64; keys.len().div_ceil(64)];
    let mut cursor = 0_usize;
    for (block, rows) in keys.chunks(BLOCK_ROWS).enumerate() {
        let base = block * BLOCK_ROWS;
        let (first, last) = (rows[0], rows[rows.len() - 1]);
        while cursor < changed.len() && changed[cursor] < first {
            cursor += 1;
        }
        if cursor >= changed.len() || changed[cursor] > last {
            continue;
        }
        let mut probe = cursor;
        while probe < changed.len() && changed[probe] <= last {
            if let Ok(offset) = rows.binary_search(&changed[probe]) {
                let row = base + offset;
                bits[row / 64] |= 1 << (row % 64);
            }
            probe += 1;
        }
    }
    bits
}

/// One row's arrival: find where it sits and mark it. This is what a
/// maintained mask pays, once per change rather than once per scan.
fn apply_one(keys: &[i64], bits: &mut [u64], key: i64) {
    if let Ok(row) = keys.binary_search(&key) {
        bits[row / 64] |= 1 << (row % 64);
    }
}

/// What a scan does with a mask it did not have to build.
fn count_survivors(bits: &[u64], rows: usize) -> usize {
    rows - bits
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum::<usize>()
}

#[test]
#[ignore = "a measurement over a large key column, not a gate"]
fn maintaining_the_mask_beats_rebuilding_it_once_queries_repeat() {
    let keys: Vec<i64> = (0..SEGMENT_ROWS)
        .map(|n| i64::try_from(n).expect("small"))
        .collect();
    for (shape, changed) in [
        ("scattered", scattered_changes()),
        ("clustered", clustered_changes()),
    ] {
        one_shape(&keys, shape, &changed);
    }
}

const CHANGED: usize = 20_000;

/// Evenly spread over the segment: every part of the key column is
/// touched.
fn scattered_changes() -> Vec<i64> {
    let stride = SEGMENT_ROWS / CHANGED;
    (0..CHANGED)
        .map(|n| i64::try_from(n * stride).expect("small"))
        .collect()
}

/// A contiguous run at the end, as recent activity leaves.
fn clustered_changes() -> Vec<i64> {
    let first = SEGMENT_ROWS - CHANGED;
    (0..CHANGED)
        .map(|n| i64::try_from(first + n).expect("small"))
        .collect()
}

fn one_shape(keys: &[i64], shape: &str, changed: &[i64]) {
    let mut rebuild = f64::MAX;
    let mut reference = Vec::new();
    for _ in 0..5 {
        let clock = Instant::now();
        reference = build_mask(keys, changed);
        rebuild = rebuild.min(clock.elapsed().as_secs_f64());
    }

    let mut apply_all = f64::MAX;
    let mut maintained = Vec::new();
    for _ in 0..5 {
        let mut bits = vec![0_u64; keys.len().div_ceil(64)];
        let clock = Instant::now();
        for key in changed {
            apply_one(keys, &mut bits, *key);
        }
        apply_all = apply_all.min(clock.elapsed().as_secs_f64());
        maintained = bits;
    }
    assert_eq!(reference, maintained, "both masks must mark the same rows");

    let mut read = f64::MAX;
    let mut survivors = 0;
    for _ in 0..5 {
        let clock = Instant::now();
        survivors = count_survivors(&maintained, SEGMENT_ROWS);
        read = read.min(clock.elapsed().as_secs_f64());
    }
    assert_eq!(survivors, SEGMENT_ROWS - CHANGED);

    println!();
    println!("{SEGMENT_ROWS} rows, {CHANGED} changed, {shape}, minimum of 5 runs");
    println!("  rebuild the mask per scan   = {:8.3} ms", rebuild * 1e3);
    println!(
        "  mark all {CHANGED} as they arrive = {:8.3} ms",
        apply_all * 1e3
    );
    println!(
        "    per changed row           = {:8.0} ns",
        apply_all * 1e9 / f64::from(u32::try_from(CHANGED).expect("small"))
    );
    println!("  read a mask already built   = {:8.3} ms", read * 1e3);
    println!();
    println!("work per second at 2,000 updates/s, by query rate:");
    println!(
        "{:>10}  {:>16}  {:>16}",
        "queries/s", "rebuild ms/s", "maintain ms/s"
    );
    let per_change = apply_all / f64::from(u32::try_from(CHANGED).expect("small"));
    for queries in [1.0_f64, 5.0, 10.0, 50.0] {
        let per_second_rebuilding = queries * rebuild * 1e3;
        let per_second_maintaining = 2_000.0 * per_change * 1e3 + queries * read * 1e3;
        println!("{queries:>10.0}  {per_second_rebuilding:>16.1}  {per_second_maintaining:>16.1}");
    }
}
