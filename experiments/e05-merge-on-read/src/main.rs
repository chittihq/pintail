//! e05: Merge-on-read (the "FINAL tax") shoot-out.
//!
//! Contested question: how much does always-on versioned dedup cost, and how
//! much of it does ClickHouse-style overlap classification recover?
//!
//! Setup: 20M-row table, pk-dense, split into 8 disjoint sorted base segments
//! (version 0) plus one "hot tail" segment of updates (version 1) covering an
//! overlap fraction f of the keys. Query: SUM(latest amount).
//!
//! Variants:
//!   REF  fully-compacted single segment (the unique_keys fast path floor)
//!   A    9-way heap merge over everything (the naive always-FINAL path)
//!   B    classified: each disjoint base segment 2-way merged with its tail
//!        slice only (sweep-line classification)
//!   C    scan + patch: full-speed scan of base plus O(U) corrections
//!        (the endgame of non-overlapping classification for dense keys)

use common::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const N: usize = 20_000_000;
const SEGS: usize = 8;

struct Segment {
    pk: Vec<u64>,
    ver: Vec<u8>,
    amt: Vec<i64>,
}

fn main() {
    println!("e05-merge-on-read  N = {N}, base segments = {SEGS}");
    let mut r = Lcg::new(42);
    let base_amt: Vec<i64> = (0..N).map(|_| 100 + r.below(999_900) as i64).collect();

    for f in [0.001f64, 0.01, 0.1] {
        let u = (N as f64 * f) as usize;
        println!("\n== overlap fraction f = {f} ({u} updated keys) ==");

        // Base: 8 disjoint pk-range segments.
        let per = N / SEGS;
        let base: Vec<Segment> = (0..SEGS)
            .map(|s| {
                let lo = s * per;
                let hi = if s == SEGS - 1 { N } else { lo + per };
                Segment {
                    pk: (lo as u64..hi as u64).collect(),
                    ver: vec![0u8; hi - lo],
                    amt: base_amt[lo..hi].to_vec(),
                }
            })
            .collect();

        // Tail: u distinct random pks, sorted, version 1, new amounts.
        let mut rr = Lcg::new(1000 + (f * 1e6) as u64);
        let mut tail_pks: Vec<u64> = (0..u * 2).map(|_| rr.below(N as u64)).collect();
        tail_pks.sort_unstable();
        tail_pks.dedup();
        tail_pks.truncate(u);
        let tail = Segment {
            amt: tail_pks
                .iter()
                .map(|&pk| base_amt[pk as usize] + 1_000)
                .collect(),
            ver: vec![1u8; tail_pks.len()],
            pk: tail_pks,
        };

        // Precompute the fully-compacted array (outside timing) for REF.
        let mut compacted = base_amt.clone();
        for (i, &pk) in tail.pk.iter().enumerate() {
            compacted[pk as usize] = tail.amt[i];
        }

        let mut rs = vec![];

        rs.push(bench(
            "REF: fully compacted scan (unique_keys floor)",
            || {
                let mut s = 0i64;
                for &v in &compacted {
                    s += v;
                }
                s as u64
            },
        ));

        rs.push(bench("A: naive 9-way heap merge (always-FINAL)", || {
            let mut segs: Vec<&Segment> = base.iter().collect();
            segs.push(&tail);
            // Heap entries: Reverse((pk, inverted_version, seg_idx, elem_idx))
            let mut heap: BinaryHeap<Reverse<(u64, u8, usize, usize)>> =
                BinaryHeap::with_capacity(segs.len());
            for (si, seg) in segs.iter().enumerate() {
                if !seg.pk.is_empty() {
                    heap.push(Reverse((seg.pk[0], 255 - seg.ver[0], si, 0)));
                }
            }
            let mut s = 0i64;
            let mut last_pk = u64::MAX;
            while let Some(Reverse((pk, _iv, si, idx))) = heap.pop() {
                if pk != last_pk {
                    s += segs[si].amt[idx];
                    last_pk = pk;
                }
                let next = idx + 1;
                if next < segs[si].pk.len() {
                    heap.push(Reverse((
                        segs[si].pk[next],
                        255 - segs[si].ver[next],
                        si,
                        next,
                    )));
                }
            }
            s as u64
        }));

        rs.push(bench("B: classified per-segment 2-way merge", || {
            let mut s = 0i64;
            for seg in &base {
                let lo_pk = seg.pk[0];
                let hi_pk = *seg.pk.last().unwrap();
                let tlo = tail.pk.partition_point(|&p| p < lo_pk);
                let thi = tail.pk.partition_point(|&p| p <= hi_pk);
                let (mut i, mut j) = (0usize, tlo);
                let bl = seg.pk.len();
                while i < bl || j < thi {
                    if j >= thi || (i < bl && seg.pk[i] < tail.pk[j]) {
                        s += seg.amt[i];
                        i += 1;
                    } else if i < bl && seg.pk[i] == tail.pk[j] {
                        s += tail.amt[j]; // higher version wins
                        i += 1;
                        j += 1;
                    } else {
                        s += tail.amt[j]; // insert-only key
                        j += 1;
                    }
                }
            }
            s as u64
        }));

        rs.push(bench(
            "C: scan + patch corrections (dense-pk endgame)",
            || {
                let mut s = 0i64;
                for seg in &base {
                    for &v in &seg.amt {
                        s += v;
                    }
                }
                for (i, &pk) in tail.pk.iter().enumerate() {
                    s += tail.amt[i] - base_amt[pk as usize];
                }
                s as u64
            },
        ));

        check_consistency(&rs);
    }
}
