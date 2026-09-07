use crate::Data;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};
pub const NAMES: &[&str] = &[
    "reference-tree-membership",
    "hash-membership",
    "sorted-binary-membership",
    "dense-membership-bitmap",
    "bloom-negative-filter",
    "hash-partition-membership",
    "sorted-probe-merge",
    "parallel-hash-membership",
    "eytzinger-search",
    "radix-membership",
    "memoized-probe-outcomes",
];
fn truth(probe: Option<usize>, found: bool, has_null: bool, empty: bool) -> i128 {
    if empty {
        0
    } else if probe.is_none() {
        2
    } else if found {
        1
    } else if has_null {
        2
    } else {
        0
    }
}
fn eytzinger(sorted: &[usize], tree: &mut [usize], at: usize, pos: &mut usize) {
    if at < tree.len() {
        eytzinger(sorted, tree, at * 2, pos);
        tree[at] = sorted[*pos];
        *pos += 1;
        eytzinger(sorted, tree, at * 2 + 1, pos);
    }
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let source: Vec<_> = d
        .rows
        .iter()
        .filter(|r| r.id.is_multiple_of(3))
        .map(|r| r.valid.then_some(r.key))
        .collect();
    let probes: Vec<_> = d.rows.iter().map(|r| r.valid.then_some(r.key)).collect();
    let has_null = source.contains(&None);
    let values: Vec<_> = source.iter().flatten().copied().collect();
    let empty = source.is_empty();
    let found: Vec<bool> = match v {
        0 => {
            let set: BTreeSet<_> = values.into_iter().collect();
            probes
                .iter()
                .map(|p| p.is_some_and(|x| set.contains(&x)))
                .collect()
        }
        1 | 4 | 7 => {
            let set: HashSet<_> = values.into_iter().collect();
            let mut bloom = vec![0u64; 1024];
            for &x in &set {
                let h = x.wrapping_mul(0x9e3779b1) % 65536;
                bloom[h / 64] |= 1 << (h % 64);
            }
            let test = |p: &Option<usize>| {
                p.is_some_and(|x| {
                    let h = x.wrapping_mul(0x9e3779b1) % 65536;
                    (v != 4 || bloom[h / 64] & (1 << (h % 64)) != 0) && set.contains(&x)
                })
            };
            if v == 7 {
                probes.par_iter().map(test).collect()
            } else {
                probes.iter().map(test).collect()
            }
        }
        2 => {
            let mut sorted = values;
            sorted.sort_unstable();
            sorted.dedup();
            probes
                .iter()
                .map(|p| p.is_some_and(|x| sorted.binary_search(&x).is_ok()))
                .collect()
        }
        3 => {
            let mut bits = vec![0u64; d.domain.div_ceil(64)];
            for x in values {
                bits[x / 64] |= 1 << (x % 64);
            }
            probes
                .iter()
                .map(|p| p.is_some_and(|x| bits[x / 64] & (1 << (x % 64)) != 0))
                .collect()
        }
        5 => {
            let mut sets = vec![HashSet::new(); 16];
            for x in values {
                sets[x % 16].insert(x);
            }
            probes
                .iter()
                .map(|p| p.is_some_and(|x| sets[x % 16].contains(&x)))
                .collect()
        }
        6 => {
            let mut sorted = values;
            sorted.sort_unstable();
            sorted.dedup();
            let mut p: Vec<_> = probes
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.map(|x| (x, i)))
                .collect();
            p.sort_unstable();
            let mut out = vec![false; probes.len()];
            let mut j = 0;
            for (x, i) in p {
                while j < sorted.len() && sorted[j] < x {
                    j += 1;
                }
                out[i] = j < sorted.len() && sorted[j] == x;
            }
            out
        }
        8 => {
            let mut sorted = values;
            sorted.sort_unstable();
            sorted.dedup();
            let mut tree = vec![0; sorted.len() + 1];
            eytzinger(&sorted, &mut tree, 1, &mut 0);
            probes
                .iter()
                .map(|p| {
                    p.is_some_and(|x| {
                        let mut i = 1;
                        while i < tree.len() {
                            if tree[i] == x {
                                return true;
                            }
                            i = i * 2 + usize::from(x > tree[i]);
                        }
                        false
                    })
                })
                .collect()
        }
        9 => {
            let mut sorted: Vec<_> = values.into_iter().map(|x| x as u64).collect();
            crate::distinct::radix(&mut sorted);
            sorted.dedup();
            probes
                .iter()
                .map(|p| p.is_some_and(|x| sorted.binary_search(&(x as u64)).is_ok()))
                .collect()
        }
        10 => {
            let set: HashSet<_> = values.into_iter().collect();
            let mut memo = HashMap::new();
            probes
                .iter()
                .map(|p| p.is_some_and(|x| *memo.entry(x).or_insert_with(|| set.contains(&x))))
                .collect()
        }
        _ => unreachable!(),
    };
    probes
        .into_iter()
        .zip(found)
        .flat_map(|(p, f)| {
            let v = truth(p, f, has_null, empty);
            [v, if v == 2 { 2 } else { 1 - v }]
        })
        .collect()
}
