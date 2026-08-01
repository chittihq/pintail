//! e18: Small materialized aggregates per block, with dirty-block fallback.
//!
//! Papers: Moerkotte — "Small Materialized Aggregates: A Light Weight
//! Index Structure for Data Warehousing" (VLDB 1998); Lang et al. —
//! "Data Blocks: Hybrid OLTP and OLAP on Compressed Storage using both
//! Vectorization and Compilation" (SIGMOD 2016, HyPer), which attaches
//! SMAs to immutable compressed blocks.
//!
//! Contested question (a replica-specific product lever, not a kernel
//! tune): PTSEG blocks are immutable once flushed, so (count, sum, min,
//! max) per column — and a per-dict-code sub-cube for low-cardinality
//! columns — can be computed once at write. A scan-shaped aggregate then
//! reads stats for clean blocks and scans only blocks whose rows are
//! superseded by newer CDC versions. ClickHouse scans ~20M rows for Q3
//! in ~240 ms; a mostly-clean replica should answer from metadata and go
//! UNDER that floor while staying exact. What does the crossover look
//! like as the dirty fraction grows?
//!
//! Variants (identical checksums per query; 20M rows, 64k blocks,
//! status u8 x5 / amount i64; dirty blocks chosen deterministically):
//!  - full fused scan (reference, e01 kernel shape)
//!  - SMA + dirty-block scan at 0% / 1% / 5% / 20% / 100% dirty
//!  Query A: global SUM, COUNT, MIN, MAX of amount.
//!  Query B: per-status SUM + COUNT (the Q3 shape) via per-code sub-SMAs.

use common::*;
use rayon::prelude::*;

const ROWS: usize = N_ORDERS;
const BLOCK: usize = 1 << 16;

struct BlockSma {
    count: u64,
    sum: i64,
    min: i64,
    max: i64,
    status_count: [u64; N_STATUS],
    status_sum: [i64; N_STATUS],
}

fn build_smas(orders: &Orders) -> Vec<BlockSma> {
    orders
        .status
        .chunks(BLOCK)
        .zip(orders.amount.chunks(BLOCK))
        .map(|(status, amount)| {
            let mut sma = BlockSma {
                count: status.len() as u64,
                sum: 0,
                min: i64::MAX,
                max: i64::MIN,
                status_count: [0; N_STATUS],
                status_sum: [0; N_STATUS],
            };
            for (status, amount) in status.iter().zip(amount) {
                sma.sum += amount;
                sma.min = sma.min.min(*amount);
                sma.max = sma.max.max(*amount);
                sma.status_count[*status as usize] += 1;
                sma.status_sum[*status as usize] += amount;
            }
            sma
        })
        .collect()
}

/// Deterministic dirty set: roughly `percent`% of blocks marked dirty.
fn dirty_mask(blocks: usize, percent: u64) -> Vec<bool> {
    let mut rng = Lcg::new(0xe18 + percent);
    (0..blocks).map(|_| rng.below(100) < percent).collect()
}

fn checksum_global(count: u64, sum: i64, min: i64, max: i64) -> u64 {
    count ^ (sum as u64).rotate_left(16) ^ (min as u64).rotate_left(32) ^ (max as u64).rotate_left(48)
}

fn checksum_grouped(counts: &[u64; N_STATUS], sums: &[i64; N_STATUS]) -> u64 {
    counts
        .iter()
        .zip(sums)
        .enumerate()
        .fold(0u64, |acc, (code, (count, sum))| {
            acc ^ ((code as u64) + 1).wrapping_mul(count ^ (*sum as u64))
        })
}

fn scan_global(orders: &Orders) -> u64 {
    let (count, sum, min, max) = orders
        .amount
        .par_chunks(BLOCK)
        .map(|amounts| {
            let mut acc = (0u64, 0i64, i64::MAX, i64::MIN);
            for amount in amounts {
                acc.0 += 1;
                acc.1 += amount;
                acc.2 = acc.2.min(*amount);
                acc.3 = acc.3.max(*amount);
            }
            acc
        })
        .reduce(
            || (0, 0, i64::MAX, i64::MIN),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2.min(b.2), a.3.max(b.3)),
        );
    checksum_global(count, sum, min, max)
}

fn sma_global(orders: &Orders, smas: &[BlockSma], dirty: &[bool]) -> u64 {
    let (count, sum, min, max) = smas
        .par_iter()
        .enumerate()
        .map(|(index, sma)| {
            if dirty[index] {
                // A dirty block's stats are stale: rescan its rows.
                let start = index * BLOCK;
                let amounts = &orders.amount[start..(start + BLOCK).min(ROWS)];
                let mut acc = (0u64, 0i64, i64::MAX, i64::MIN);
                for amount in amounts {
                    acc.0 += 1;
                    acc.1 += amount;
                    acc.2 = acc.2.min(*amount);
                    acc.3 = acc.3.max(*amount);
                }
                acc
            } else {
                (sma.count, sma.sum, sma.min, sma.max)
            }
        })
        .reduce(
            || (0, 0, i64::MAX, i64::MIN),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2.min(b.2), a.3.max(b.3)),
        );
    checksum_global(count, sum, min, max)
}

fn scan_grouped(orders: &Orders) -> u64 {
    let (counts, sums) = orders
        .status
        .par_chunks(BLOCK)
        .zip(orders.amount.par_chunks(BLOCK))
        .map(|(statuses, amounts)| {
            let mut counts = [0u64; N_STATUS];
            let mut sums = [0i64; N_STATUS];
            for (status, amount) in statuses.iter().zip(amounts) {
                counts[*status as usize] += 1;
                sums[*status as usize] += amount;
            }
            (counts, sums)
        })
        .reduce(
            || ([0; N_STATUS], [0; N_STATUS]),
            |mut a, b| {
                for code in 0..N_STATUS {
                    a.0[code] += b.0[code];
                    a.1[code] += b.1[code];
                }
                a
            },
        );
    checksum_grouped(&counts, &sums)
}

fn sma_grouped(orders: &Orders, smas: &[BlockSma], dirty: &[bool]) -> u64 {
    let (counts, sums) = smas
        .par_iter()
        .enumerate()
        .map(|(index, sma)| {
            if dirty[index] {
                let start = index * BLOCK;
                let end = (start + BLOCK).min(ROWS);
                let mut counts = [0u64; N_STATUS];
                let mut sums = [0i64; N_STATUS];
                for (status, amount) in orders.status[start..end]
                    .iter()
                    .zip(&orders.amount[start..end])
                {
                    counts[*status as usize] += 1;
                    sums[*status as usize] += amount;
                }
                (counts, sums)
            } else {
                (sma.status_count, sma.status_sum)
            }
        })
        .reduce(
            || ([0; N_STATUS], [0; N_STATUS]),
            |mut a, b| {
                for code in 0..N_STATUS {
                    a.0[code] += b.0[code];
                    a.1[code] += b.1[code];
                }
                a
            },
        );
    checksum_grouped(&counts, &sums)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e18: {ROWS} rows, {BLOCK}-row blocks, {threads} threads");
    let orders = gen_orders(ROWS, 0xe18);
    let build = std::time::Instant::now();
    let smas = build_smas(&orders);
    println!(
        "SMA build (write-time cost): {:.1} ms for {} blocks, {} bytes/block",
        build.elapsed().as_secs_f64() * 1e3,
        smas.len(),
        std::mem::size_of::<BlockSma>()
    );
    type ScanFn = fn(&Orders) -> u64;
    type SmaFn = fn(&Orders, &[BlockSma], &[bool]) -> u64;
    let queries: [(&str, ScanFn, SmaFn); 2] = [
        ("global SUM/COUNT/MIN/MAX", scan_global, sma_global),
        ("per-status SUM/COUNT (Q3 shape)", scan_grouped, sma_grouped),
    ];
    for (label, scan, sma) in queries {
        println!("== {label} ==");
        let mut results = vec![bench("full fused parallel scan", || scan(&orders))];
        for percent in [0u64, 1, 5, 20, 100] {
            let dirty = dirty_mask(smas.len(), percent);
            let actual = dirty.iter().filter(|dirty| **dirty).count();
            results.push(bench(
                &format!("SMA + dirty scan ({percent}% target, {actual} blocks)"),
                || sma(&orders, &smas, &dirty),
            ));
        }
        check_consistency(&results);
    }
}
