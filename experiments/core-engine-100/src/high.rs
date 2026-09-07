use crate::{Agg, Data, Groups, flat, low, reference_group};
use rayon::prelude::*;
use std::{cmp::Reverse, collections::BinaryHeap};
pub const NAMES: &[&str] = &[
    "reference-tree-full-sort",
    "hash-heap",
    "dense-heap",
    "sort-reduce-heap",
    "parallel-local-hash-heap",
    "key-owned-local-topk",
    "radix-shuffle-dense-partials",
    "adaptive-domain-group",
    "tree-quickselect",
    "sorted-run-merge-heap",
    "hash-quickselect",
];
pub fn top(groups: Groups, heap: bool, select: bool) -> Vec<i128> {
    let mut ranks: Vec<_> = if heap {
        let mut h = BinaryHeap::new();
        for (k, a) in groups {
            h.push((a.count == 0, Reverse(a.sum), k, a.rows, a.count));
            if h.len() > 32 {
                h.pop();
            }
        }
        h.into_vec()
    } else {
        groups
            .into_iter()
            .map(|(k, a)| (a.count == 0, Reverse(a.sum), k, a.rows, a.count))
            .collect()
    };
    if select && ranks.len() > 32 {
        ranks.select_nth_unstable(32);
        ranks.truncate(32);
    }
    ranks.sort_unstable();
    ranks.truncate(32);
    flat(
        ranks
            .into_iter()
            .map(|(_, Reverse(sum), key, rows, count)| (key, Agg { rows, count, sum })),
    )
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let r = &d.rows;
    match v {
        0 => top(reference_group(r, false), false, false),
        1 => top(low::hash(r, false), true, false),
        2 => top(low::dense(r, d.domain, false), true, false),
        3 => top(low::sorted(r, false), true, false),
        4 => top(
            low::combine(
                r.par_chunks(8192)
                    .map(|b| low::hash(b, false))
                    .collect::<Vec<_>>(),
            ),
            true,
            false,
        ),
        5 => {
            let mut parts = vec![Vec::new(); 16];
            for row in r {
                parts[row.key % 16].push(*row);
            }
            let candidates: Vec<_> = parts
                .par_iter()
                .map(|b| {
                    let map = low::hash(b, false);
                    let mut ranked: Vec<_> = map.into_iter().collect();
                    ranked.sort_unstable_by_key(|(k, a)| (a.count == 0, Reverse(a.sum), *k));
                    ranked.truncate(32);
                    ranked
                })
                .collect();
            top(candidates.into_iter().flatten().collect(), true, false)
        }
        6 => {
            let mut parts = vec![Vec::new(); 16];
            for row in r {
                parts[row.key % 16].push(*row);
            }
            let maps: Vec<_> = parts
                .par_iter()
                .enumerate()
                .map(|(p, b)| {
                    let mut slots = vec![Agg::default(); d.domain.div_ceil(16)];
                    for row in b {
                        slots[row.key / 16].add(row);
                    }
                    slots
                        .into_iter()
                        .enumerate()
                        .filter(|(_, a)| a.rows != 0)
                        .map(|(i, a)| (i * 16 + p, a))
                        .collect::<Groups>()
                })
                .collect();
            top(low::combine(maps), true, false)
        }
        7 => {
            let min = r.iter().map(|r| r.key).min().unwrap_or(0);
            let max = r.iter().map(|r| r.key).max().unwrap_or(0);
            let map = if max - min < r.len() / 2 {
                let mut slots = vec![Agg::default(); max - min + 1];
                for row in r {
                    slots[row.key - min].add(row);
                }
                slots
                    .into_iter()
                    .enumerate()
                    .filter(|(_, a)| a.rows != 0)
                    .map(|(k, a)| (k + min, a))
                    .collect()
            } else {
                low::hash(r, false)
            };
            top(map, true, false)
        }
        8 => top(reference_group(r, false), false, true),
        9 => {
            let runs: Vec<Vec<_>> = r
                .chunks(8192)
                .map(|b| low::sorted(b, false).into_iter().collect())
                .collect();
            let mut heap = BinaryHeap::new();
            for (i, run) in runs.iter().enumerate() {
                if !run.is_empty() {
                    heap.push(Reverse((run[0].0, i, 0)));
                }
            }
            let mut groups = Groups::new();
            while let Some(Reverse((key, run, pos))) = heap.pop() {
                groups.entry(key).or_default().merge(runs[run][pos].1);
                if pos + 1 < runs[run].len() {
                    heap.push(Reverse((runs[run][pos + 1].0, run, pos + 1)));
                }
            }
            top(groups, true, false)
        }
        10 => top(low::hash(r, false), false, true),
        _ => unreachable!(),
    }
}
