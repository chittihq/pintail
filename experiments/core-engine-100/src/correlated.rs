use crate::{Agg, Data, Groups, flat, low};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
pub const NAMES: &[&str] = &[
    "reference-dependent-rescan",
    "memo-distinct-outer",
    "hash-decorrelation",
    "dense-decorrelation",
    "sorted-range-lookup",
    "key-row-position-index",
    "demand-filtered-aggregate",
    "parallel-partial-decorrelation",
    "parallel-dependent-scans",
    "sorted-prefix-sums",
    "demand-bitmap-dense-fold",
];
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let outer: Vec<_> = d.rows.iter().take(128).map(|r| r.key).collect();
    let scan = |key: usize| {
        let mut a = Agg::default();
        for row in &d.rows {
            if row.key == key {
                a.add(row);
            }
        }
        a
    };
    let values: Vec<Agg> = match v {
        0 => outer.iter().map(|&k| scan(k)).collect(),
        1 => {
            let mut memo = HashMap::new();
            outer
                .iter()
                .map(|&k| *memo.entry(k).or_insert_with(|| scan(k)))
                .collect()
        }
        2 => {
            let groups = low::hash(&d.rows, false);
            outer.iter().map(|k| groups[k]).collect()
        }
        3 => {
            let groups = low::dense(&d.rows, d.domain, false);
            outer.iter().map(|k| groups[k]).collect()
        }
        4 => {
            let mut rows = d.rows.clone();
            rows.sort_unstable_by_key(|r| r.key);
            outer
                .iter()
                .map(|&k| {
                    let start = rows.partition_point(|r| r.key < k);
                    let mut a = Agg::default();
                    for row in rows[start..].iter().take_while(|r| r.key == k) {
                        a.add(row);
                    }
                    a
                })
                .collect()
        }
        5 => {
            let mut index = HashMap::<usize, Vec<usize>>::new();
            for (i, r) in d.rows.iter().enumerate() {
                index.entry(r.key).or_default().push(i);
            }
            outer
                .iter()
                .map(|k| {
                    let mut a = Agg::default();
                    for &i in &index[k] {
                        a.add(&d.rows[i]);
                    }
                    a
                })
                .collect()
        }
        6 => {
            let demand: HashSet<_> = outer.iter().copied().collect();
            let mut groups = Groups::new();
            for r in &d.rows {
                if demand.contains(&r.key) {
                    groups.entry(r.key).or_default().add(r);
                }
            }
            outer.iter().map(|k| groups[k]).collect()
        }
        7 => {
            let groups = low::combine(
                d.rows
                    .par_chunks(8192)
                    .map(|b| low::hash(b, false))
                    .collect::<Vec<_>>(),
            );
            outer.iter().map(|k| groups[k]).collect()
        }
        8 => outer.par_iter().map(|&k| scan(k)).collect(),
        9 => {
            let mut rows = d.rows.clone();
            rows.sort_unstable_by_key(|r| r.key);
            let mut prefix = vec![Agg::default()];
            for r in &rows {
                let mut a = *prefix.last().unwrap();
                a.add(r);
                prefix.push(a);
            }
            outer
                .iter()
                .map(|&k| {
                    let l = rows.partition_point(|r| r.key < k);
                    let h = rows.partition_point(|r| r.key <= k);
                    Agg {
                        rows: prefix[h].rows - prefix[l].rows,
                        count: prefix[h].count - prefix[l].count,
                        sum: prefix[h].sum - prefix[l].sum,
                    }
                })
                .collect()
        }
        10 => {
            let mut wanted = vec![false; d.domain];
            for &k in &outer {
                wanted[k] = true;
            }
            let mut groups = vec![Agg::default(); d.domain];
            for r in &d.rows {
                if wanted[r.key] {
                    groups[r.key].add(r);
                }
            }
            outer.iter().map(|&k| groups[k]).collect()
        }
        _ => unreachable!(),
    };
    flat(outer.into_iter().zip(values))
}
