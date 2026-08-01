//! e03: Top-K strategy shoot-out (top-100 of 20M by amount).
//!
//! Contested question: full sort vs select_nth vs bounded heap vs
//! cutoff-guarded heap (ClickHouse/DuckDB threshold prefiltering) vs parallel
//! local-top-K + merge.

use common::*;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const K: usize = 100;
const CHUNK: usize = 1 << 20;

fn ck_top(top: &[i64]) -> u64 {
    top.iter()
        .map(|&v| v as u64)
        .fold(0u64, |a, b| a.wrapping_add(b))
}

#[inline]
fn local_topk_guarded(vals: &[i64]) -> BinaryHeap<Reverse<i64>> {
    let mut heap: BinaryHeap<Reverse<i64>> = BinaryHeap::with_capacity(K + 1);
    let mut cutoff = i64::MIN;
    for &v in vals {
        if v > cutoff || heap.len() < K {
            heap.push(Reverse(v));
            if heap.len() > K {
                heap.pop();
            }
            if heap.len() == K {
                cutoff = heap.peek().unwrap().0;
            }
        }
    }
    heap
}

fn main() {
    println!("e03-topk  N = {N_ORDERS}, K = {K}");
    let o = gen_orders(N_ORDERS, 42);
    let amount = &o.amount;
    let mut rs = vec![];

    rs.push(bench("clone + full sort desc + take K", || {
        let mut v = amount.clone();
        v.sort_unstable_by(|a, b| b.cmp(a));
        ck_top(&v[..K])
    }));

    rs.push(bench("clone + select_nth_unstable + sort K", || {
        let mut v = amount.clone();
        v.select_nth_unstable_by(K - 1, |a, b| b.cmp(a));
        let mut top: Vec<i64> = v[..K].to_vec();
        top.sort_unstable_by(|a, b| b.cmp(a));
        ck_top(&top)
    }));

    rs.push(bench("naive bounded heap (push every row)", || {
        let mut heap: BinaryHeap<Reverse<i64>> = BinaryHeap::with_capacity(K + 1);
        for &v in amount {
            heap.push(Reverse(v));
            if heap.len() > K {
                heap.pop();
            }
        }
        let top: Vec<i64> = heap.into_iter().map(|r| r.0).collect();
        ck_top(&top)
    }));

    rs.push(bench("cutoff-guarded heap (threshold prefilter)", || {
        let heap = local_topk_guarded(amount);
        let top: Vec<i64> = heap.into_iter().map(|r| r.0).collect();
        ck_top(&top)
    }));

    rs.push(bench("parallel: per-chunk guarded heaps + merge", || {
        let mut candidates: Vec<i64> = amount
            .par_chunks(CHUNK)
            .map(|chunk| {
                local_topk_guarded(chunk)
                    .into_iter()
                    .map(|r| r.0)
                    .collect::<Vec<i64>>()
            })
            .reduce(Vec::new, |mut a, b| {
                a.extend_from_slice(&b);
                a
            });
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        ck_top(&candidates[..K])
    }));

    check_consistency(&rs);
}
