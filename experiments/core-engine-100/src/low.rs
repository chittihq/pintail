use crate::{Agg, Data, Groups, Row, flat, reference_group};
use rayon::prelude::*;
use std::collections::HashMap;
pub const NAMES: &[&str] = &[
    "reference-tree-group",
    "hash-group",
    "dense-group",
    "four-independent-dense-lanes",
    "sort-run-reduce",
    "key-partition-hash",
    "parallel-local-hash",
    "parallel-local-dense",
    "adaptive-small-map",
    "group-membership-bitmaps",
    "counting-scatter-groups",
];
pub fn hash(rows: &[Row], low: bool) -> Groups {
    let mut m: HashMap<usize, Agg> = HashMap::new();
    for r in rows {
        m.entry(if low { r.low } else { r.key }).or_default().add(r);
    }
    m.into_iter().collect()
}
pub fn dense(rows: &[Row], domain: usize, low: bool) -> Groups {
    let mut slots = vec![Agg::default(); domain];
    for r in rows {
        slots[if low { r.low } else { r.key }].add(r);
    }
    slots
        .into_iter()
        .enumerate()
        .filter(|(_, a)| a.rows != 0)
        .collect()
}
pub fn sorted(rows: &[Row], low: bool) -> Groups {
    let mut x = rows.to_vec();
    x.sort_unstable_by_key(|r| if low { r.low } else { r.key });
    let mut out = Groups::new();
    let mut i = 0;
    while i < x.len() {
        let key = if low { x[i].low } else { x[i].key };
        let mut a = Agg::default();
        while i < x.len() && (if low { x[i].low } else { x[i].key }) == key {
            a.add(&x[i]);
            i += 1;
        }
        out.insert(key, a);
    }
    out
}
pub fn combine(parts: impl IntoIterator<Item = Groups>) -> Groups {
    let mut out = Groups::new();
    for p in parts {
        for (k, a) in p {
            out.entry(k).or_default().merge(a);
        }
    }
    out
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let r = &d.rows;
    let g = match v {
        0 => reference_group(r, true),
        1 => hash(r, true),
        2 => dense(r, d.low_domain, true),
        3 => {
            let mut lanes = vec![vec![Agg::default(); d.low_domain]; 4];
            for (i, row) in r.iter().enumerate() {
                lanes[i % 4][row.low].add(row);
            }
            let mut out = vec![Agg::default(); d.low_domain];
            for lane in lanes {
                for (i, a) in lane.into_iter().enumerate() {
                    out[i].merge(a);
                }
            }
            out.into_iter()
                .enumerate()
                .filter(|(_, a)| a.rows != 0)
                .collect()
        }
        4 => sorted(r, true),
        5 => {
            let mut buckets = vec![Vec::new(); 16];
            for row in r {
                buckets[row.low % 16].push(*row);
            }
            combine(buckets.into_iter().map(|b| hash(&b, true)))
        }
        6 => combine(
            r.par_chunks(8192)
                .map(|b| hash(b, true))
                .collect::<Vec<_>>(),
        ),
        7 => combine(
            r.par_chunks(8192)
                .map(|b| dense(b, d.low_domain, true))
                .collect::<Vec<_>>(),
        ),
        8 => {
            let mut small: Vec<(usize, Agg)> = Vec::new();
            let mut large: Option<HashMap<usize, Agg>> = None;
            for row in r {
                if let Some(m) = &mut large {
                    m.entry(row.low).or_default().add(row);
                } else if let Some((_, a)) = small.iter_mut().find(|(k, _)| *k == row.low) {
                    a.add(row);
                } else if small.len() < 8 {
                    let mut a = Agg::default();
                    a.add(row);
                    small.push((row.low, a));
                } else {
                    let mut m: HashMap<_, _> = small.drain(..).collect();
                    m.entry(row.low).or_default().add(row);
                    large = Some(m);
                }
            }
            large.map_or_else(|| small.into_iter().collect(), |m| m.into_iter().collect())
        }
        9 => {
            let width = r.len().div_ceil(64);
            let mut bits = vec![vec![0u64; width]; d.low_domain];
            for (i, row) in r.iter().enumerate() {
                bits[row.low][i / 64] |= 1 << (i % 64);
            }
            let mut out = Groups::new();
            for (k, b) in bits.into_iter().enumerate() {
                let mut a = Agg::default();
                for (w, mut mask) in b.into_iter().enumerate() {
                    while mask != 0 {
                        a.add(&r[w * 64 + mask.trailing_zeros() as usize]);
                        mask &= mask - 1;
                    }
                }
                if a.rows != 0 {
                    out.insert(k, a);
                }
            }
            out
        }
        10 => {
            let mut count = vec![0; d.low_domain];
            for row in r {
                count[row.low] += 1;
            }
            let mut offsets = vec![0; d.low_domain + 1];
            for i in 0..d.low_domain {
                offsets[i + 1] = offsets[i] + count[i];
            }
            let mut pos = offsets.clone();
            let mut ids = vec![0; r.len()];
            for (i, row) in r.iter().enumerate() {
                ids[pos[row.low]] = i;
                pos[row.low] += 1;
            }
            let mut out = Groups::new();
            for k in 0..d.low_domain {
                let mut a = Agg::default();
                for &i in &ids[offsets[k]..offsets[k + 1]] {
                    a.add(&r[i]);
                }
                if a.rows != 0 {
                    out.insert(k, a);
                }
            }
            out
        }
        _ => unreachable!(),
    };
    flat(g)
}
