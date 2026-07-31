//! e12: Composite-key comparison strategy for k-way merges.
//!
//! 8 sorted runs × 2.5M rows of (tenant_id u32, placed_at i64, id u64) merged
//! to one stream. Variants: typed tuple comparator | 20-byte normalized
//! memcmp key (DuckDB "These Rows Are Made for Sorting") | packed
//! (u128, u64) two-level compare. Normalized keys are precomputed per run —
//! in pintail they'd be built once at segment write; the encode cost is
//! measured separately and reported.
//!
//! Offset-value coding (Graefe) is future work — noted, not implemented.

use common::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const RUNS: usize = 8;
const PER_RUN: usize = 2_500_000;

#[derive(Clone)]
struct Row {
    tenant: u32,
    ts: i64,
    id: u64,
}

#[inline]
fn normalize(row: &Row) -> [u8; 20] {
    let mut key = [0u8; 20];
    key[..4].copy_from_slice(&row.tenant.to_be_bytes());
    key[4..12].copy_from_slice(&((row.ts as u64) ^ (1u64 << 63)).to_be_bytes());
    key[12..20].copy_from_slice(&row.id.to_be_bytes());
    key
}

#[inline]
fn pack(row: &Row) -> (u128, u64) {
    (((row.tenant as u128) << 64) | (((row.ts as u64) ^ (1u64 << 63)) as u128), row.id)
}

fn main() {
    println!("e12-normalized-keys  {RUNS} runs x {PER_RUN} rows");
    let mut rng = Lcg::new(42);
    let mut runs: Vec<Vec<Row>> = Vec::with_capacity(RUNS);
    let mut next_id = 0u64;
    for _ in 0..RUNS {
        let mut rows: Vec<Row> = (0..PER_RUN)
            .map(|_| {
                next_id += 1;
                Row {
                    tenant: rng.below(1_000) as u32,
                    ts: 1_600_000_000_000 + rng.below(100_000_000_000) as i64 - 50_000_000_000,
                    id: next_id,
                }
            })
            .collect();
        rows.sort_unstable_by_key(|r| (r.tenant, r.ts, r.id));
        runs.push(rows);
    }

    // precompute normalized/packed forms (segment-write-time work in pintail)
    let mut encode_ms = 0.0;
    let started = std::time::Instant::now();
    let normalized: Vec<Vec<[u8; 20]>> =
        runs.iter().map(|run| run.iter().map(normalize).collect()).collect();
    encode_ms += started.elapsed().as_secs_f64() * 1e3;
    let packed: Vec<Vec<(u128, u64)>> =
        runs.iter().map(|run| run.iter().map(pack).collect()).collect();
    println!("normalized-key encode cost (one-time, at write): {encode_ms:.1} ms for 20M rows");

    // checksum = order-sensitive fold over merged ids
    let mut rs = vec![];

    rs.push(bench("typed tuple comparator heap", || {
        let mut heap: BinaryHeap<Reverse<(u32, i64, u64, usize, usize)>> = BinaryHeap::new();
        for (s, run) in runs.iter().enumerate() {
            let r = &run[0];
            heap.push(Reverse((r.tenant, r.ts, r.id, s, 0)));
        }
        let mut ck = 0u64;
        while let Some(Reverse((_, _, id, s, i))) = heap.pop() {
            ck = ck.wrapping_mul(31).wrapping_add(id);
            let next = i + 1;
            if next < runs[s].len() {
                let r = &runs[s][next];
                heap.push(Reverse((r.tenant, r.ts, r.id, s, next)));
            }
        }
        ck
    }));

    rs.push(bench("normalized [u8;20] memcmp heap", || {
        let mut heap: BinaryHeap<Reverse<([u8; 20], usize, usize)>> = BinaryHeap::new();
        for (s, keys) in normalized.iter().enumerate() {
            heap.push(Reverse((keys[0], s, 0)));
        }
        let mut ck = 0u64;
        while let Some(Reverse((key, s, i))) = heap.pop() {
            ck = ck.wrapping_mul(31).wrapping_add(u64::from_be_bytes(key[12..20].try_into().unwrap()));
            let next = i + 1;
            if next < normalized[s].len() {
                heap.push(Reverse((normalized[s][next], s, next)));
            }
        }
        ck
    }));

    rs.push(bench("packed (u128, u64) two-level heap", || {
        let mut heap: BinaryHeap<Reverse<(u128, u64, usize, usize)>> = BinaryHeap::new();
        for (s, keys) in packed.iter().enumerate() {
            let (hi, id) = keys[0];
            heap.push(Reverse((hi, id, s, 0)));
        }
        let mut ck = 0u64;
        while let Some(Reverse((_, id, s, i))) = heap.pop() {
            ck = ck.wrapping_mul(31).wrapping_add(id);
            let next = i + 1;
            if next < packed[s].len() {
                let (hi, nid) = packed[s][next];
                heap.push(Reverse((hi, nid, s, next)));
            }
        }
        ck
    }));

    check_consistency(&rs);
}
