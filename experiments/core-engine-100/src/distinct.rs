use crate::Data;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
pub const NAMES: &[&str] = &[
    "reference-tree-pairs",
    "hash-pairs",
    "per-group-hashsets",
    "sort-deduplicate",
    "dense-group-bitmaps",
    "parallel-local-bitmaps",
    "radix-sort-pairs",
    "sorted-run-union",
    "per-group-sort",
    "adaptive-inline-sets",
    "sparse-word-bitmaps",
];
fn code(value: i64) -> usize {
    (value + 10000) as usize
}
pub fn radix(x: &mut Vec<u64>) {
    let mut scratch = vec![0; x.len()];
    for shift in (0..64).step_by(8) {
        let mut count = [0usize; 256];
        for &v in x.iter() {
            count[((v >> shift) & 255) as usize] += 1;
        }
        let mut offsets = [0usize; 256];
        for i in 1..256 {
            offsets[i] = offsets[i - 1] + count[i - 1];
        }
        for &v in x.iter() {
            let k = ((v >> shift) & 255) as usize;
            scratch[offsets[k]] = v;
            offsets[k] += 1;
        }
        std::mem::swap(x, &mut scratch);
    }
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let pairs: Vec<_> = d
        .rows
        .iter()
        .filter(|r| r.valid)
        .map(|r| (r.low, code(r.value)))
        .collect();
    let mut count = vec![0usize; d.low_domain];
    match v {
        0 => {
            for (k, _) in pairs.into_iter().collect::<BTreeSet<_>>() {
                count[k] += 1;
            }
        }
        1 => {
            for (k, _) in pairs.into_iter().collect::<HashSet<_>>() {
                count[k] += 1;
            }
        }
        2 => {
            let mut sets = vec![HashSet::new(); d.low_domain];
            for (k, x) in pairs {
                sets[k].insert(x);
            }
            for (k, s) in sets.into_iter().enumerate() {
                count[k] = s.len();
            }
        }
        3 => {
            let mut p = pairs;
            p.sort_unstable();
            p.dedup();
            for (k, _) in p {
                count[k] += 1;
            }
        }
        4 | 5 => {
            let width = 20001usize.div_ceil(64);
            let build = |p: &[(usize, usize)]| {
                let mut bits = vec![0u64; d.low_domain * width];
                for &(k, x) in p {
                    bits[k * width + x / 64] |= 1 << (x % 64);
                }
                bits
            };
            let bits = if v == 4 {
                build(&pairs)
            } else {
                let mut combined = vec![0u64; d.low_domain * width];
                for part in pairs.par_chunks(8192).map(build).collect::<Vec<_>>() {
                    for (a, b) in combined.iter_mut().zip(part) {
                        *a |= b;
                    }
                }
                combined
            };
            for (k, chunk) in bits.chunks(width).enumerate() {
                count[k] = chunk.iter().map(|w| w.count_ones() as usize).sum();
            }
        }
        6 => {
            let mut p: Vec<_> = pairs
                .into_iter()
                .map(|(k, x)| ((k as u64) << 32) | x as u64)
                .collect();
            radix(&mut p);
            p.dedup();
            for x in p {
                count[(x >> 32) as usize] += 1;
            }
        }
        7 => {
            let runs: Vec<_> = pairs
                .chunks(4096)
                .map(|b| {
                    let mut b = b.to_vec();
                    b.sort_unstable();
                    b.dedup();
                    b
                })
                .collect();
            let mut heap = BinaryHeap::new();
            for (i, r) in runs.iter().enumerate() {
                if !r.is_empty() {
                    heap.push(Reverse((r[0], i, 0)));
                }
            }
            let mut last = None;
            while let Some(Reverse((pair, i, j))) = heap.pop() {
                if last != Some(pair) {
                    count[pair.0] += 1;
                    last = Some(pair);
                }
                if j + 1 < runs[i].len() {
                    heap.push(Reverse((runs[i][j + 1], i, j + 1)));
                }
            }
        }
        8 => {
            let mut sets = vec![Vec::new(); d.low_domain];
            for (k, x) in pairs {
                sets[k].push(x);
            }
            for (k, mut s) in sets.into_iter().enumerate() {
                s.sort_unstable();
                s.dedup();
                count[k] = s.len();
            }
        }
        9 => {
            let mut small = vec![Vec::new(); d.low_domain];
            let mut large: Vec<Option<HashSet<usize>>> = vec![None; d.low_domain];
            for (k, x) in pairs {
                if let Some(s) = &mut large[k] {
                    s.insert(x);
                } else if !small[k].contains(&x) {
                    small[k].push(x);
                    if small[k].len() > 32 {
                        large[k] = Some(small[k].drain(..).collect());
                    }
                }
            }
            for k in 0..d.low_domain {
                count[k] = large[k].as_ref().map_or(small[k].len(), HashSet::len);
            }
        }
        10 => {
            let mut words = HashMap::<(usize, usize), u64>::new();
            for (k, x) in pairs {
                *words.entry((k, x / 64)).or_default() |= 1 << (x % 64);
            }
            for ((k, _), bits) in words {
                count[k] += bits.count_ones() as usize;
            }
        }
        _ => unreachable!(),
    };
    let present: HashSet<_> = d.rows.iter().map(|r| r.low).collect();
    count
        .into_iter()
        .enumerate()
        .filter(|(k, _)| present.contains(k))
        .flat_map(|(k, c)| [k as i128, c as i128])
        .collect()
}
