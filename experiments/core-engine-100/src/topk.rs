use crate::Data;
use rayon::prelude::*;
use std::{cmp::Reverse, collections::BinaryHeap};
pub const NAMES: &[&str] = &[
    "reference-full-sort",
    "bounded-heap",
    "quickselect-prefix",
    "sorted-small-vector",
    "chunk-local-heaps",
    "parallel-local-selection",
    "value-bucket-selection",
    "radix-score-order",
    "tournament-tree",
    "block-bound-pruning",
    "buffered-selection",
];
type Rank = (bool, Reverse<i64>, usize);
fn heap_top(rows: impl IntoIterator<Item = Rank>) -> Vec<Rank> {
    let mut h = BinaryHeap::new();
    for r in rows {
        h.push(r);
        if h.len() > 32 {
            h.pop();
        }
    }
    h.into_vec()
}
fn select(mut r: Vec<Rank>) -> Vec<Rank> {
    if r.len() > 32 {
        r.select_nth_unstable(32);
        r.truncate(32);
    }
    r
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let ranks: Vec<Rank> = d
        .rows
        .iter()
        .map(|r| (!r.valid, Reverse(if r.valid { r.value } else { 0 }), r.id))
        .collect();
    let mut best = match v {
        0 => ranks,
        1 => heap_top(ranks),
        2 => select(ranks),
        3 => {
            let mut best = Vec::new();
            for rank in ranks {
                let at = best.partition_point(|x| *x < rank);
                if at < 32 {
                    best.insert(at, rank);
                    best.truncate(32);
                }
            }
            best
        }
        4 => heap_top(ranks.chunks(4096).flat_map(|b| heap_top(b.iter().copied()))),
        5 => heap_top(
            ranks
                .par_chunks(8192)
                .map(|b| select(b.to_vec()))
                .collect::<Vec<_>>()
                .into_iter()
                .flatten(),
        ),
        6 => {
            let mut buckets = vec![Vec::new(); 202];
            for r in ranks {
                let bucket = if r.0 {
                    201
                } else {
                    ((10000 - r.1.0) / 100) as usize
                };
                buckets[bucket].push(r);
            }
            let mut result = Vec::new();
            for mut bucket in buckets {
                bucket.sort_unstable();
                result.extend(bucket.into_iter().take(32 - result.len()));
                if result.len() == 32 {
                    break;
                }
            }
            result
        }
        7 => {
            let mut encoded: Vec<_> = ranks
                .iter()
                .map(|r| {
                    let score = if r.0 { 20001 } else { 10000 - r.1.0 };
                    ((score as u64) << 32) | r.2 as u64
                })
                .collect();
            crate::distinct::radix(&mut encoded);
            encoded
                .into_iter()
                .take(32)
                .map(|v| {
                    let score = (v >> 32) as i64;
                    let null = score == 20001;
                    (
                        null,
                        Reverse(if null { 0 } else { 10000 - score }),
                        v as u32 as usize,
                    )
                })
                .collect()
        }
        8 => {
            if ranks.is_empty() {
                Vec::new()
            } else {
                let size = ranks.len().next_power_of_two();
                let sentinel = (true, Reverse(i64::MIN), usize::MAX);
                let mut tree = vec![sentinel; size * 2];
                tree[size..size + ranks.len()].copy_from_slice(&ranks);
                for i in (1..size).rev() {
                    tree[i] = tree[i * 2].min(tree[i * 2 + 1]);
                }
                let mut out = Vec::new();
                for _ in 0..32.min(ranks.len()) {
                    let winner = tree[1];
                    out.push(winner);
                    let mut i = 1;
                    while i < size {
                        i = if tree[i * 2] == winner {
                            i * 2
                        } else {
                            i * 2 + 1
                        };
                    }
                    tree[i] = sentinel;
                    while i > 1 {
                        i /= 2;
                        tree[i] = tree[i * 2].min(tree[i * 2 + 1]);
                    }
                }
                out
            }
        }
        9 => {
            let mut heap = BinaryHeap::new();
            for block in ranks.chunks(1024) {
                let bound = block.iter().min().unwrap();
                if heap.len() == 32 && heap.peek().unwrap() <= bound {
                    continue;
                }
                for &rank in block {
                    heap.push(rank);
                    if heap.len() > 32 {
                        heap.pop();
                    }
                }
            }
            heap.into_vec()
        }
        10 => {
            let mut buffer = Vec::new();
            for rank in ranks {
                buffer.push(rank);
                if buffer.len() == 256 {
                    buffer = select(buffer);
                }
            }
            select(buffer)
        }
        _ => unreachable!(),
    };
    best.sort_unstable();
    best.truncate(32);
    best.into_iter()
        .flat_map(|(null, Reverse(value), id)| [id as i128, i128::from(value), i128::from(!null)])
        .collect()
}
