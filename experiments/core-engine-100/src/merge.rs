use crate::{Data, Row};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
pub const NAMES: &[&str] = &[
    "reference-tree-latest",
    "hash-latest",
    "sort-version-reduce",
    "sorted-two-way",
    "dense-version-slots",
    "binary-patch-base",
    "sparse-overlay-map",
    "block-overlap-search",
    "parallel-key-ranges",
    "visibility-bitmap",
    "copy-winning-runs",
];
#[derive(Clone, Copy)]
pub struct Version {
    pub row: Row,
    pub version: u64,
    pub dead: bool,
}
fn inputs(d: &Data) -> (Vec<Version>, Vec<Version>) {
    if let Some(history) = &d.history {
        let base: Vec<_> = history.iter().filter(|v| v.version == 1).copied().collect();
        let mut tail = BTreeMap::new();
        for v in history.iter().filter(|v| v.version != 1) {
            tail.entry(v.row.id).and_modify(|old| *old = newer(*old, *v)).or_insert(*v);
        }
        return (base, tail.into_values().collect());
    }
    let base: Vec<_> = d
        .rows
        .iter()
        .map(|&row| Version {
            row,
            version: 2,
            dead: false,
        })
        .collect();
    let stride = match d.scenario {
        1 => 5,
        2 => 2,
        _ => 100,
    };
    let update = base
        .iter()
        .step_by(stride)
        .map(|v| {
            let mut r = v.row;
            r.value += 17;
            Version {
                row: r,
                version: if r.id.is_multiple_of(7) { 1 } else { 3 },
                dead: r.id.is_multiple_of(11),
            }
        })
        .collect();
    (base, update)
}
fn newer(a: Version, b: Version) -> Version {
    if b.version > a.version { b } else { a }
}
fn output(rows: impl IntoIterator<Item = Version>) -> Vec<i128> {
    rows.into_iter()
        .filter(|r| !r.dead)
        .flat_map(|v| {
            [
                v.row.id as i128,
                v.row.key as i128,
                v.row.low as i128,
                i128::from(v.row.value),
                i128::from(v.row.valid),
            ]
        })
        .collect()
}
fn merge(a: &[Version], b: &[Version]) -> Vec<Version> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len() + b.len());
    while i < a.len() && j < b.len() {
        if a[i].row.id < b[j].row.id {
            out.push(a[i]);
            i += 1;
        } else if a[i].row.id > b[j].row.id {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(newer(a[i], b[j]));
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let (a, b) = inputs(d);
    match v {
        0 => {
            let mut map = BTreeMap::new();
            for x in a.iter().chain(&b) {
                map.entry(x.row.id)
                    .and_modify(|old| *old = newer(*old, *x))
                    .or_insert(*x);
            }
            output(map.into_values())
        }
        1 => {
            let mut map = HashMap::new();
            for x in a.iter().chain(&b) {
                map.entry(x.row.id)
                    .and_modify(|old| *old = newer(*old, *x))
                    .or_insert(*x);
            }
            let mut x: Vec<_> = map.into_values().collect();
            x.sort_unstable_by_key(|v| v.row.id);
            output(x)
        }
        2 => {
            let mut x = a;
            x.extend(b);
            x.sort_unstable_by_key(|v| (v.row.id, std::cmp::Reverse(v.version)));
            x.dedup_by_key(|v| v.row.id);
            output(x)
        }
        3 => output(merge(&a, &b)),
        4 => {
            let max = a.iter().chain(&b).map(|v|v.row.id+1).max().unwrap_or(0);
            let mut slots = vec![None; max];
            for x in a.iter().chain(&b) {
                let slot = &mut slots[x.row.id];
                *slot = Some(slot.map_or(*x, |old| newer(old, *x)));
            }
            output(slots.into_iter().flatten())
        }
        5 => {
            let mut x = a;
            for update in b {
                match x.binary_search_by_key(&update.row.id, |v|v.row.id) { Ok(i)=>x[i]=newer(x[i],update), Err(i)=>x.insert(i,update) }
            }
            output(x)
        }
        6 => {
            let mut updates: HashMap<_,_> = b.into_iter().map(|v|(v.row.id,v)).collect();
            let mut out:Vec<_> = a.into_iter().map(|x|updates.remove(&x.row.id).map_or(x,|u|newer(x,u))).collect();
            out.extend(updates.into_values());out.sort_unstable_by_key(|v|v.row.id);output(out)
        }
        7 => {
            let mut out = Vec::new();
            for block in a.chunks(8192) {
                let lo = block.first().unwrap().row.id;
                let hi = block.last().unwrap().row.id;
                let l = b.partition_point(|v| v.row.id < lo);
                let h = b.partition_point(|v| v.row.id <= hi);
                if l == h {
                    out.extend_from_slice(block);
                } else {
                    out.extend(merge(block, &b[l..h]));
                }
            }
            append_new(&a,&b,&mut out);
            output(out)
        }
        8 => {
            let chunks: Vec<_> = a
                .par_chunks(8192)
                .map(|block| {
                    let l = b.partition_point(|v| v.row.id < block[0].row.id);
                    let h = b.partition_point(|v| v.row.id <= block.last().unwrap().row.id);
                    merge(block, &b[l..h])
                })
                .collect();
            let mut out:Vec<_>=chunks.into_iter().flatten().collect();
            append_new(&a,&b,&mut out);output(out)
        }
        9 => {
            let mut live = vec![u64::MAX; a.len().div_ceil(64)];
            let mut patches = Vec::new();
            for update in b {
                let Ok(i) = a.binary_search_by_key(&update.row.id, |v|v.row.id) else {patches.push(update);continue;};
                if update.version > a[i].version {
                    live[i / 64] &= !(1 << (i % 64));
                    patches.push(update);
                }
            }
            let mut x: Vec<_> = a
                .into_iter()
                .enumerate()
                .filter(|(i, _)| live[i / 64] & (1 << (i % 64)) != 0)
                .map(|(_, v)| v)
                .collect();
            x.extend(patches);
            x.sort_unstable_by_key(|v| v.row.id);
            output(x)
        }
        10 => {
            let mut out = Vec::with_capacity(a.len());
            let mut start = 0;
            for update in b {
                let i=a.partition_point(|v|v.row.id<update.row.id);
                out.extend_from_slice(&a[start..i]);
                if i<a.len()&&a[i].row.id==update.row.id {out.push(newer(a[i],update));start=i+1;}else{out.push(update);start=i;}
            }
            out.extend_from_slice(&a[start..]);
            output(out)
        }
        _ => unreachable!(),
    }
}

fn append_new(a:&[Version],b:&[Version],out:&mut Vec<Version>){let mut changed=false;for &v in b{if a.binary_search_by_key(&v.row.id,|r|r.row.id).is_err(){out.push(v);changed=true;}}if changed{out.sort_unstable_by_key(|v|v.row.id);}}
