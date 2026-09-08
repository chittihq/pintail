use crate::{Data, Row};
use rayon::prelude::*;
pub const NAMES: &[&str] = &[
    "reference-row-filter",
    "selective-predicate-first",
    "selection-vector",
    "full-bitmap",
    "block-bitmap-set-bits",
    "parallel-block-filter",
    "build-zone-maps",
    "build-sorted-value-index",
    "build-low-key-buckets",
    "columnar-filter-projection",
    "two-phase-block-survivors",
];
fn pass(r: &Row) -> bool {
    r.valid && r.value >= 8000 && r.low < 4
}
fn emit<'a>(rows: impl Iterator<Item = &'a Row>) -> Vec<i128> {
    rows.flat_map(|r| [r.id as i128, i128::from(r.value)])
        .collect()
}
pub fn run(v: usize, d: &Data) -> Vec<i128> {
    let rows = &d.rows;
    match v {
        0 => emit(
            rows.iter()
                .filter(|r| r.low < 4 && r.valid && r.value >= 8000),
        ),
        1 => emit(rows.iter().filter(|r| pass(r))),
        2 => {
            let ids: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| pass(r))
                .map(|(i, _)| i)
                .collect();
            emit(ids.iter().map(|&i| &rows[i]))
        }
        3 => {
            let mut bits = vec![0u64; rows.len().div_ceil(64)];
            for (i, r) in rows.iter().enumerate() {
                if pass(r) {
                    bits[i / 64] |= 1 << (i % 64);
                }
            }
            emit(
                rows.iter()
                    .enumerate()
                    .filter(|(i, _)| bits[i / 64] & (1 << (i % 64)) != 0)
                    .map(|(_, r)| r),
            )
        }
        4 => {
            let mut out = Vec::new();
            for block in rows.chunks(64) {
                let mut bits = 0u64;
                for (i, r) in block.iter().enumerate() {
                    bits |= u64::from(pass(r)) << i;
                }
                while bits != 0 {
                    let i = bits.trailing_zeros() as usize;
                    out.extend([block[i].id as i128, i128::from(block[i].value)]);
                    bits &= bits - 1;
                }
            }
            out
        }
        5 => rows
            .par_chunks(8192)
            .map(|b| emit(b.iter().filter(|r| pass(r))))
            .collect::<Vec<_>>()
            .concat(),
        6 => {
            let summaries: Vec<_> = rows
                .chunks(1024)
                .map(|b| b.iter().map(|r| r.value).max().unwrap_or(i64::MIN))
                .collect();
            let mut out = Vec::new();
            for (b, max) in rows.chunks(1024).zip(summaries) {
                if max >= 8000 {
                    out.extend(emit(b.iter().filter(|r| pass(r))));
                }
            }
            out
        }
        7 => {
            let mut index: Vec<_> = rows.iter().enumerate().map(|(i, r)| (r.value, i)).collect();
            index.sort_unstable();
            let start = index.partition_point(|(value, _)| *value < 8000);
            let mut ids: Vec<_> = index[start..]
                .iter()
                .map(|(_, id)| *id)
                .filter(|&i| rows[i].valid && rows[i].low < 4)
                .collect();
            ids.sort_unstable();
            emit(ids.into_iter().map(|i| &rows[i]))
        }
        8 => {
            let mut buckets = vec![Vec::new(); d.low_domain];
            for (i, r) in rows.iter().enumerate() {
                buckets[r.low].push(i);
            }
            let mut ids: Vec<_> = buckets
                .iter()
                .take(4)
                .flatten()
                .copied()
                .filter(|&i| pass(&rows[i]))
                .collect();
            ids.sort_unstable();
            emit(ids.into_iter().map(|i| &rows[i]))
        }
        9 => {
            let values: Vec<_> = rows.iter().map(|r| r.value).collect();
            let low: Vec<_> = rows.iter().map(|r| r.low).collect();
            let valid: Vec<_> = rows.iter().map(|r| r.valid).collect();
            let mut out = Vec::new();
            for i in 0..rows.len() {
                if valid[i] && values[i] >= 8000 && low[i] < 4 {
                    out.extend([rows[i].id as i128, i128::from(values[i])]);
                }
            }
            out
        }
        10 => {
            let mut out = Vec::new();
            for b in rows.chunks(4096) {
                let ids: Vec<_> = b
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.valid && r.value >= 8000)
                    .map(|(i, _)| i)
                    .collect();
                out.extend(emit(ids.iter().map(|&i| &b[i]).filter(|r| r.low < 4)));
            }
            out
        }
        _ => unreachable!(),
    }
}
