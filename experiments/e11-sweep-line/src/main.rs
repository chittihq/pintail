//! e11: Granule-level overlap classification (the ClickHouse PartsSplitter idea
//! at PTSEG granularity) with a level-0 memtable overlay, under two update
//! patterns: clustered (recent-hot keys — the realistic CDC shape) and
//! scattered (uniform random keys — the adversarial shape).
//!
//! Query: SUM(latest amount). Sources: 8 disjoint base segments (sparse index =
//! first pk per 64K granule) + 1 tail segment (updates, version 1) + 1 memtable
//! (newest updates, version 2).

use common::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const N: usize = 20_000_000;
const SEGS: usize = 8;
const GRANULE: usize = 65_536;
const TAIL_U: usize = 200_000;
const MEM_U: usize = 50_000;

struct Run {
    pk: Vec<u64>,
    ver: Vec<u8>,
    amt: Vec<i64>,
}

fn sorted_updates(rng: &mut Lcg, count: usize, lo: u64, hi: u64, ver: u8, base_amt: &[i64]) -> Run {
    let mut pks: Vec<u64> = (0..count * 2).map(|_| lo + rng.below(hi - lo)).collect()
    ;
    pks.sort_unstable();
    pks.dedup();
    pks.truncate(count);
    Run {
        amt: pks.iter().map(|&p| base_amt[p as usize] + 1_000).collect(),
        ver: vec![ver; pks.len()],
        pk: pks,
    }
}

fn merge_all(runs: &[&Run]) -> i64 {
    let mut heap: BinaryHeap<Reverse<(u64, u8, usize, usize)>> = BinaryHeap::new();
    for (si, run) in runs.iter().enumerate() {
        if !run.pk.is_empty() {
            heap.push(Reverse((run.pk[0], 255 - run.ver[0], si, 0)));
        }
    }
    let mut sum = 0i64;
    let mut last = u64::MAX;
    while let Some(Reverse((pk, _iv, si, idx))) = heap.pop() {
        if pk != last {
            sum += runs[si].amt[idx];
            last = pk;
        }
        let next = idx + 1;
        if next < runs[si].pk.len() {
            heap.push(Reverse((runs[si].pk[next], 255 - runs[si].ver[next], si, next)));
        }
    }
    sum
}

/// 3-source merge over one granule range: base slice + tail slice + mem slice.
/// mem (ver 2) beats tail (ver 1) beats base (ver 0).
fn merge_granule(base: (&[u64], &[i64]), tail: (&[u64], &[i64]), mem: (&[u64], &[i64])) -> i64 {
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize)
    ;
    let mut sum = 0i64;
    let mut last = u64::MAX;
    loop {
        let bp = base.0.get(i).copied().unwrap_or(u64::MAX);
        let tp = tail.0.get(j).copied().unwrap_or(u64::MAX);
        let mp = mem.0.get(k).copied().unwrap_or(u64::MAX);
        let min = bp.min(tp).min(mp);
        if min == u64::MAX {
            break;
        }
        // highest version at this pk wins; advance every cursor at min
        if mp == min {
            if min != last {
                sum += mem.1[k];
                last = min;
            }
            k += 1;
        }
        if tp == min {
            if min != last {
                sum += tail.1[j];
                last = min;
            }
            j += 1;
        }
        if bp == min {
            if min != last {
                sum += base.1[i];
                last = min;
            }
            i += 1;
        }
    }
    sum
}

fn slice_range<'a>(run: &'a Run, lo: u64, hi: u64) -> (&'a [u64], &'a [i64]) {
    let a = run.pk.partition_point(|&p| p < lo);
    let b = run.pk.partition_point(|&p| p <= hi);
    (&run.pk[a..b], &run.amt[a..b])
}

fn scenario(label: &str, tail: &Run, mem: &Run, base: &[Run]) {
    println!("\n== updates: {label} ({} tail + {} memtable rows) ==", tail.pk.len(), mem.pk.len());
    let mut rs = vec![];

    let all: Vec<&Run> = base.iter().chain([tail, mem]).collect();
    rs.push(bench("A: full 10-way heap merge (naive FINAL)", || merge_all(&all) as u64));

    rs.push(bench("B: granule-classified merge", || {
        let mut sum = 0i64;
        let mut overlapping = 0usize;
        for seg in base {
            for (g, chunk) in seg.pk.chunks(GRANULE).enumerate() {
                let lo = chunk[0];
                let hi = *chunk.last().unwrap();
                let t = slice_range(tail, lo, hi);
                let m = slice_range(mem, lo, hi);
                let start = g * GRANULE;
                let amts = &seg.amt[start..start + chunk.len()];
                if t.0.is_empty() && m.0.is_empty() {
                    for &v in amts {
                        sum += v;
                    }
                } else {
                    overlapping += 1;
                    sum += merge_granule((chunk, amts), t, m);
                }
            }
        }
        std::hint::black_box(overlapping);
        sum as u64
    }));

    // report overlap fraction once
    let mut overlapping = 0usize;
    let mut total = 0usize;
    for seg in base {
        for chunk in seg.pk.chunks(GRANULE) {
            total += 1;
            let lo = chunk[0];
            let hi = *chunk.last().unwrap();
            if !slice_range(tail, lo, hi).0.is_empty() || !slice_range(mem, lo, hi).0.is_empty() {
                overlapping += 1;
            }
        }
    }
    println!("   granules overlapping: {overlapping}/{total}");
    check_consistency(&rs);
}

fn main() {
    println!("e11-sweep-line  N = {N}, {SEGS} base segments, granule = {GRANULE}");
    let mut rng = Lcg::new(42);
    let base_amt: Vec<i64> = (0..N).map(|_| 100 + rng.below(999_900) as i64).collect();
    let per = N / SEGS;
    let base: Vec<Run> = (0..SEGS)
        .map(|s| {
            let lo = s * per;
            let hi = if s == SEGS - 1 { N } else { lo + per };
            Run {
                pk: (lo as u64..hi as u64).collect(),
                ver: vec![0; hi - lo],
                amt: base_amt[lo..hi].to_vec(),
            }
        })
        .collect();

    // clustered: updates hit the newest 5% of the keyspace (CDC reality)
    let hot_lo = (N as f64 * 0.95) as u64;
    let tail_hot = sorted_updates(&mut rng, TAIL_U, hot_lo, N as u64, 1, &base_amt);
    let mem_hot = sorted_updates(&mut rng, MEM_U, hot_lo, N as u64, 2, &base_amt);
    scenario("clustered in newest 5% of keyspace", &tail_hot, &mem_hot, &base);

    // scattered: uniform updates (adversarial for classification)
    let tail_uniform = sorted_updates(&mut rng, TAIL_U, 0, N as u64, 1, &base_amt);
    let mem_uniform = sorted_updates(&mut rng, MEM_U, 0, N as u64, 2, &base_amt);
    scenario("scattered uniformly", &tail_uniform, &mem_uniform, &base);
}
