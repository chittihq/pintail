use crate::{Agg, Data, Groups, Row, flat, low};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
pub const NAMES: &[&str] = &[
    "reference-tree-bucket-join",
    "hash-bucket-join",
    "dense-bucket-join",
    "sorted-merge-join",
    "sorted-binary-join",
    "radix-partition-join",
    "parallel-hash-probe",
    "parallel-dense-probe",
    "bloom-prefilter-hash",
    "build-side-demand-filter",
    "factorized-fact-aggregate",
];
pub fn dimension(d: &Data) -> Vec<(usize, usize)> {
    let mut out: Vec<_> = d
        .rows
        .iter()
        .filter(|r| r.id < d.domain.min(512) && !r.id.is_multiple_of(7))
        .map(|r| (r.key, r.low))
        .collect();
    out.sort_unstable();
    out
}

fn hash_join(rows: &[Row], index: &HashMap<usize, Vec<usize>>) -> Groups {
    let mut out = Groups::new();
    for row in rows {
        if let Some(groups) = index.get(&row.key) {
            for &g in groups {
                out.entry(g).or_default().add(row);
            }
        }
    }
    out
}
fn dense_join(rows: &[Row], index: &[Vec<usize>]) -> Groups {
    let mut slots = [Agg::default(); 64];
    for row in rows {
        for &g in &index[row.key] {
            slots[g].add(row);
        }
    }
    slots
        .into_iter()
        .enumerate()
        .filter(|(_, a)| a.rows != 0)
        .collect()
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let dim = dimension(d);
    let r = &d.rows;
    let out = match v {
        0 => {
            let mut index: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (k, g) in dim {
                index.entry(k).or_default().push(g);
            }
            let mut out = Groups::new();
            for row in r {
                if let Some(groups) = index.get(&row.key) {
                    for &g in groups {
                        out.entry(g).or_default().add(row);
                    }
                }
            }
            out
        }
        1 | 6 | 8 | 9 => {
            let demand: HashSet<_> = if v == 9 {
                r.iter().map(|r| r.key).collect()
            } else {
                HashSet::new()
            };
            let mut index: HashMap<usize, Vec<usize>> = HashMap::new();
            let mut bloom = vec![0u64; 256];
            for (k, g) in dim {
                if v == 9 && !demand.contains(&k) {
                    continue;
                }
                index.entry(k).or_default().push(g);
                let h = k.wrapping_mul(0x9e3779b1) % 16384;
                bloom[h / 64] |= 1 << (h % 64);
            }
            if v == 6 {
                low::combine(
                    r.par_chunks(8192)
                        .map(|b| hash_join(b, &index))
                        .collect::<Vec<_>>(),
                )
            } else if v == 8 {
                let filtered: Vec<_> = r
                    .iter()
                    .filter(|r| {
                        let h = r.key.wrapping_mul(0x9e3779b1) % 16384;
                        bloom[h / 64] & (1 << (h % 64)) != 0
                    })
                    .copied()
                    .collect();
                hash_join(&filtered, &index)
            } else {
                hash_join(r, &index)
            }
        }
        2 | 7 => {
            let mut index = vec![Vec::new(); d.domain];
            for (k, g) in dim {
                index[k].push(g);
            }
            if v == 7 {
                low::combine(
                    r.par_chunks(8192)
                        .map(|b| dense_join(b, &index))
                        .collect::<Vec<_>>(),
                )
            } else {
                dense_join(r, &index)
            }
        }
        3 => {
            let mut facts = r.to_vec();
            facts.sort_unstable_by_key(|r| r.key);
            let mut i = 0;
            let mut out = Groups::new();
            for row in facts {
                while i < dim.len() && dim[i].0 < row.key {
                    i += 1;
                }
                let mut j = i;
                while j < dim.len() && dim[j].0 == row.key {
                    out.entry(dim[j].1).or_default().add(&row);
                    j += 1;
                }
            }
            out
        }
        4 => {
            let mut out = Groups::new();
            for row in r {
                let start = dim.partition_point(|(k, _)| *k < row.key);
                for &(_, g) in dim[start..].iter().take_while(|(k, _)| *k == row.key) {
                    out.entry(g).or_default().add(row);
                }
            }
            out
        }
        5 => {
            let mut facts = vec![Vec::new(); 16];
            let mut dims = vec![HashMap::<usize, Vec<usize>>::new(); 16];
            for row in r {
                facts[row.key % 16].push(*row);
            }
            for (k, g) in dim {
                dims[k % 16].entry(k).or_default().push(g);
            }
            low::combine(
                facts
                    .par_iter()
                    .zip(dims.par_iter())
                    .map(|(f, i)| hash_join(f, i))
                    .collect::<Vec<_>>(),
            )
        }
        10 => {
            let grouped = low::hash(r, false);
            let mut out = Groups::new();
            for (k, g) in dim {
                if let Some(a) = grouped.get(&k) {
                    out.entry(g).or_default().merge(*a);
                }
            }
            out
        }
        _ => unreachable!(),
    };
    flat(out)
}
