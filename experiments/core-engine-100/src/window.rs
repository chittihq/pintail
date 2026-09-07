use crate::{Data, Row};
use rayon::prelude::*;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, VecDeque},
};
pub const NAMES: &[&str] = &[
    "reference-frame-rescan",
    "prefix-sum-monotone-min",
    "running-sum-monotone-min",
    "segment-tree-range",
    "sparse-table-min-prefix",
    "two-stack-aggregate-queue",
    "block-min-prefix",
    "parallel-halo-windows",
    "square-root-range-blocks",
    "ordered-multiset-window",
    "lazy-min-heap",
];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cell {
    sum: i128,
    count: u64,
    min: i64,
}
impl Default for Cell {
    fn default() -> Self {
        Self {
            sum: 0,
            count: 0,
            min: i64::MAX,
        }
    }
}
impl Cell {
    fn of(r: &Row) -> Self {
        if r.valid {
            Self {
                sum: i128::from(r.value),
                count: 1,
                min: r.value,
            }
        } else {
            Self::default()
        }
    }
    fn merge(self, b: Self) -> Self {
        Self {
            sum: self.sum + b.sum,
            count: self.count + b.count,
            min: self.min.min(b.min),
        }
    }
}
fn fold(c: &[Cell]) -> Cell {
    c.iter().copied().fold(Cell::default(), Cell::merge)
}
fn prefixes(c: &[Cell]) -> Vec<Cell> {
    let mut out = vec![Cell::default()];
    for &x in c {
        out.push(out.last().copied().unwrap().merge(x));
    }
    out
}
fn range(prefix: &[Cell], lo: usize, hi: usize, min: i64) -> Cell {
    Cell {
        sum: prefix[hi].sum - prefix[lo].sum,
        count: prefix[hi].count - prefix[lo].count,
        min,
    }
}
fn monotone(c: &[Cell], w: usize, prefix: bool) -> Vec<Cell> {
    let p = prefix.then(|| prefixes(c));
    let mut deque = VecDeque::<usize>::new();
    let mut state = Cell::default();
    let mut out = Vec::new();
    for (i, x) in c.iter().enumerate() {
        state.sum += x.sum;
        state.count += x.count;
        if i >= w {
            state.sum -= c[i - w].sum;
            state.count -= c[i - w].count;
        }
        while deque.front().is_some_and(|&j| j + w <= i) {
            deque.pop_front();
        }
        while deque.back().is_some_and(|&j| c[j].min >= x.min) {
            deque.pop_back();
        }
        deque.push_back(i);
        let min = c[*deque.front().unwrap()].min;
        out.push(p.as_ref().map_or(Cell { min, ..state }, |p| {
            range(p, (i + 1).saturating_sub(w), i + 1, min)
        }));
    }
    out
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let c: Vec<_> = d.rows.iter().map(Cell::of).collect();
    let w = if d.scenario == 1 { 256 } else { 64 };
    let out = match v {
        0 => (0..c.len())
            .map(|i| fold(&c[(i + 1).saturating_sub(w)..=i]))
            .collect(),
        1 => monotone(&c, w, true),
        2 => monotone(&c, w, false),
        3 => {
            let n = c.len().max(1).next_power_of_two();
            let mut tree = vec![Cell::default(); 2 * n];
            tree[n..n + c.len()].copy_from_slice(&c);
            for i in (1..n).rev() {
                tree[i] = tree[i * 2].merge(tree[i * 2 + 1]);
            }
            (0..c.len())
                .map(|i| {
                    let mut l = n + (i + 1).saturating_sub(w);
                    let mut r = n + i + 1;
                    let mut a = Cell::default();
                    while l < r {
                        if l % 2 == 1 {
                            a = a.merge(tree[l]);
                            l += 1;
                        }
                        if r % 2 == 1 {
                            r -= 1;
                            a = a.merge(tree[r]);
                        }
                        l /= 2;
                        r /= 2;
                    }
                    a
                })
                .collect()
        }
        4 => {
            let p = prefixes(&c);
            let mut levels = vec![c.iter().map(|x| x.min).collect::<Vec<_>>()];
            let mut width = 2;
            while width <= w && width <= c.len() {
                let prev = levels.last().unwrap();
                levels.push(
                    (0..=c.len() - width)
                        .map(|i| prev[i].min(prev[i + width / 2]))
                        .collect(),
                );
                width *= 2;
            }
            (0..c.len())
                .map(|i| {
                    let lo = (i + 1).saturating_sub(w);
                    let len = i + 1 - lo;
                    let level = len.ilog2() as usize;
                    let width = 1 << level;
                    range(
                        &p,
                        lo,
                        i + 1,
                        levels[level][lo].min(levels[level][i + 1 - width]),
                    )
                })
                .collect()
        }
        5 => {
            let (mut front, mut back) = (Vec::<(Cell, Cell)>::new(), Vec::<(Cell, Cell)>::new());
            let mut out = Vec::new();
            for x in c {
                let a = back.last().map_or(x, |(_, a)| a.merge(x));
                back.push((x, a));
                if front.len() + back.len() > w {
                    if front.is_empty() {
                        while let Some((x, _)) = back.pop() {
                            let a = front.last().map_or(x, |(_, a)| a.merge(x));
                            front.push((x, a));
                        }
                    }
                    front.pop();
                }
                out.push(
                    front
                        .last()
                        .map_or(Cell::default(), |(_, a)| *a)
                        .merge(back.last().map_or(Cell::default(), |(_, a)| *a)),
                );
            }
            out
        }
        6 => {
            let p = prefixes(&c);
            let mins: Vec<_> = c.chunks(32).map(|b| fold(b).min).collect();
            (0..c.len())
                .map(|i| {
                    let lo = (i + 1).saturating_sub(w);
                    let mut at = lo;
                    let mut min = i64::MAX;
                    while at <= i {
                        if at % 32 == 0 && at + 32 <= i + 1 {
                            min = min.min(mins[at / 32]);
                            at += 32;
                        } else {
                            min = min.min(c[at].min);
                            at += 1;
                        }
                    }
                    range(&p, lo, i + 1, min)
                })
                .collect()
        }
        7 => (0..c.len().div_ceil(8192))
            .into_par_iter()
            .map(|block| {
                let start = block * 8192;
                let lo = start.saturating_sub(w - 1);
                let hi = (start + 8192).min(c.len());
                monotone(&c[lo..hi], w, false)
                    .into_iter()
                    .skip(start - lo)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .concat(),
        8 => {
            let blocks: Vec<_> = c.chunks(16).map(fold).collect();
            (0..c.len())
                .map(|i| {
                    let mut at = (i + 1).saturating_sub(w);
                    let mut a = Cell::default();
                    while at <= i {
                        if at % 16 == 0 && at + 16 <= i + 1 {
                            a = a.merge(blocks[at / 16]);
                            at += 16;
                        } else {
                            a = a.merge(c[at]);
                            at += 1;
                        }
                    }
                    a
                })
                .collect()
        }
        9 => {
            let mut set = BTreeMap::<i64, usize>::new();
            let mut a = Cell::default();
            let mut out = Vec::new();
            for (i, x) in c.iter().enumerate() {
                a.sum += x.sum;
                a.count += x.count;
                *set.entry(x.min).or_default() += 1;
                if i >= w {
                    let old = c[i - w];
                    a.sum -= old.sum;
                    a.count -= old.count;
                    let count = set.get_mut(&old.min).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        set.remove(&old.min);
                    }
                }
                out.push(Cell {
                    min: *set.first_key_value().unwrap().0,
                    ..a
                });
            }
            out
        }
        10 => {
            let mut heap = BinaryHeap::new();
            let mut a = Cell::default();
            let mut out = Vec::new();
            for (i, x) in c.iter().enumerate() {
                a.sum += x.sum;
                a.count += x.count;
                heap.push(Reverse((x.min, i)));
                if i >= w {
                    a.sum -= c[i - w].sum;
                    a.count -= c[i - w].count;
                }
                while heap.peek().is_some_and(|Reverse((_, j))| *j + w <= i) {
                    heap.pop();
                }
                out.push(Cell {
                    min: heap.peek().unwrap().0.0,
                    ..a
                });
            }
            out
        }
        _ => unreachable!(),
    };
    out.into_iter()
        .flat_map(|x| {
            [
                x.sum,
                i128::from(x.count),
                if x.count == 0 { 0 } else { i128::from(x.min) },
            ]
        })
        .collect()
}
