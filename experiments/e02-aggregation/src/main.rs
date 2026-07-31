//! e02: Aggregation strategy shoot-out.
//!
//! Contested questions:
//!  - low cardinality: hash table vs direct-array (dictionary-code) accumulators
//!  - high cardinality: thread-local + merge (DuckDB/CH orthodoxy) vs shared
//!    atomics ("Global Hash Tables Strike Back!", PVLDB 2025) vs sequential
//!
//! Shapes: GROUP BY status (5), GROUP BY region×status (40), GROUP BY user_id (200k).

use common::*;
use hashbrown::HashMap;
use rayon::prelude::*;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

const CHUNK: usize = 1 << 20;

#[inline]
fn ck_entry(key: u64, sum: i64, cnt: u64) -> u64 {
    (key + 1).wrapping_mul((sum as u64) ^ cnt)
}

fn main() {
    println!("e02-aggregation  N = {N_ORDERS}");
    let o = gen_orders(N_ORDERS, 42);

    group_by_status(&o);
    group_by_region_status(&o);
    group_by_user(&o);
}

fn group_by_status(o: &Orders) {
    println!("\n== GROUP BY status (5 groups): SUM(amount), COUNT(*) ==");
    let status = &o.status;
    let amount = &o.amount;
    let mut rs = vec![];

    rs.push(bench("hashbrown HashMap<u8,(sum,cnt)>", || {
        let mut m: HashMap<u8, (i64, u64)> = HashMap::with_capacity(16);
        for (&s, &v) in status.iter().zip(amount) {
            let e = m.entry(s).or_insert((0, 0));
            e.0 += v;
            e.1 += 1;
        }
        let mut ck = 0u64;
        for (k, (s, c)) in &m {
            ck = ck.wrapping_add(ck_entry(*k as u64, *s, *c));
        }
        ck
    }));

    rs.push(bench("direct array[256] by dict code", || {
        let mut acc = [(0i64, 0u64); 256];
        for (&s, &v) in status.iter().zip(amount) {
            let a = &mut acc[s as usize];
            a.0 += v;
            a.1 += 1;
        }
        let mut ck = 0u64;
        for (k, (s, c)) in acc.iter().enumerate() {
            if *c > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, *s, *c));
            }
        }
        ck
    }));

    rs.push(bench("parallel: thread-local arrays + merge (rayon)", || {
        let merged = status
            .par_chunks(CHUNK)
            .zip(amount.par_chunks(CHUNK))
            .fold(
                || [(0i64, 0u64); 256],
                |mut acc, (ss, vv)| {
                    for (&s, &v) in ss.iter().zip(vv) {
                        let a = &mut acc[s as usize];
                        a.0 += v;
                        a.1 += 1;
                    }
                    acc
                },
            )
            .reduce(
                || [(0i64, 0u64); 256],
                |mut a, b| {
                    for (x, y) in a.iter_mut().zip(b.iter()) {
                        x.0 += y.0;
                        x.1 += y.1;
                    }
                    a
                },
            );
        let mut ck = 0u64;
        for (k, (s, c)) in merged.iter().enumerate() {
            if *c > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, *s, *c));
            }
        }
        ck
    }));

    check_consistency(&rs);
}

fn group_by_region_status(o: &Orders) {
    println!("\n== GROUP BY region × status (40 groups): SUM(amount), COUNT(*) ==");
    let region = &o.region;
    let status = &o.status;
    let amount = &o.amount;
    let mut rs = vec![];

    rs.push(bench("hashbrown HashMap<u16,(sum,cnt)>", || {
        let mut m: HashMap<u16, (i64, u64)> = HashMap::with_capacity(64);
        for ((&r, &s), &v) in region.iter().zip(status).zip(amount) {
            let key = (r as u16) * 5 + s as u16;
            let e = m.entry(key).or_insert((0, 0));
            e.0 += v;
            e.1 += 1;
        }
        let mut ck = 0u64;
        for (k, (s, c)) in &m {
            ck = ck.wrapping_add(ck_entry(*k as u64, *s, *c));
        }
        ck
    }));

    rs.push(bench("packed direct array[40] (r*5+s)", || {
        let mut acc = [(0i64, 0u64); 40];
        for ((&r, &s), &v) in region.iter().zip(status).zip(amount) {
            let a = &mut acc[r as usize * 5 + s as usize];
            a.0 += v;
            a.1 += 1;
        }
        let mut ck = 0u64;
        for (k, (s, c)) in acc.iter().enumerate() {
            if *c > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, *s, *c));
            }
        }
        ck
    }));

    rs.push(bench("parallel: thread-local array[40] + merge", || {
        let merged = region
            .par_chunks(CHUNK)
            .zip(status.par_chunks(CHUNK))
            .zip(amount.par_chunks(CHUNK))
            .fold(
                || [(0i64, 0u64); 40],
                |mut acc, ((rr, ss), vv)| {
                    for ((&r, &s), &v) in rr.iter().zip(ss).zip(vv) {
                        let a = &mut acc[r as usize * 5 + s as usize];
                        a.0 += v;
                        a.1 += 1;
                    }
                    acc
                },
            )
            .reduce(
                || [(0i64, 0u64); 40],
                |mut a, b| {
                    for (x, y) in a.iter_mut().zip(b.iter()) {
                        x.0 += y.0;
                        x.1 += y.1;
                    }
                    a
                },
            );
        let mut ck = 0u64;
        for (k, (s, c)) in merged.iter().enumerate() {
            if *c > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, *s, *c));
            }
        }
        ck
    }));

    check_consistency(&rs);
}

fn group_by_user(o: &Orders) {
    println!("\n== GROUP BY user_id (200k groups): SUM(amount), COUNT(*) ==");
    let uid = &o.user_id;
    let amount = &o.amount;
    let n = N_USERS as usize;
    let mut rs = vec![];

    rs.push(bench("hashbrown HashMap<u32,(sum,cnt)> sequential", || {
        let mut m: HashMap<u32, (i64, u64)> = HashMap::with_capacity(n * 2);
        for (&u, &v) in uid.iter().zip(amount) {
            let e = m.entry(u).or_insert((0, 0));
            e.0 += v;
            e.1 += 1;
        }
        let mut ck = 0u64;
        for (k, (s, c)) in &m {
            ck = ck.wrapping_add(ck_entry(*k as u64, *s, *c));
        }
        ck
    }));

    rs.push(bench("dense arrays (perfect hash) sequential", || {
        let mut sums = vec![0i64; n];
        let mut cnts = vec![0u64; n];
        for (&u, &v) in uid.iter().zip(amount) {
            sums[u as usize] += v;
            cnts[u as usize] += 1;
        }
        let mut ck = 0u64;
        for k in 0..n {
            if cnts[k] > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, sums[k], cnts[k]));
            }
        }
        ck
    }));

    rs.push(bench("parallel: thread-local dense arrays + merge", || {
        let (sums, cnts) = uid
            .par_chunks(CHUNK)
            .zip(amount.par_chunks(CHUNK))
            .fold(
                || (vec![0i64; n], vec![0u64; n]),
                |(mut sums, mut cnts), (uu, vv)| {
                    for (&u, &v) in uu.iter().zip(vv) {
                        sums[u as usize] += v;
                        cnts[u as usize] += 1;
                    }
                    (sums, cnts)
                },
            )
            .reduce(
                || (vec![0i64; n], vec![0u64; n]),
                |(mut sa, mut ca), (sb, cb)| {
                    for i in 0..n {
                        sa[i] += sb[i];
                        ca[i] += cb[i];
                    }
                    (sa, ca)
                },
            );
        let mut ck = 0u64;
        for k in 0..n {
            if cnts[k] > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, sums[k], cnts[k]));
            }
        }
        ck
    }));

    let atomic_sums: Vec<AtomicI64> = (0..n).map(|_| AtomicI64::new(0)).collect();
    let atomic_cnts: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
    rs.push(bench("parallel: shared dense atomics (relaxed)", || {
        atomic_sums.par_iter().for_each(|a| a.store(0, Relaxed));
        atomic_cnts.par_iter().for_each(|a| a.store(0, Relaxed));
        uid.par_chunks(CHUNK)
            .zip(amount.par_chunks(CHUNK))
            .for_each(|(uu, vv)| {
                for (&u, &v) in uu.iter().zip(vv) {
                    atomic_sums[u as usize].fetch_add(v, Relaxed);
                    atomic_cnts[u as usize].fetch_add(1, Relaxed);
                }
            });
        let mut ck = 0u64;
        for k in 0..n {
            let c = atomic_cnts[k].load(Relaxed);
            if c > 0 {
                ck = ck.wrapping_add(ck_entry(k as u64, atomic_sums[k].load(Relaxed), c));
            }
        }
        ck
    }));

    rs.push(bench("parallel: thread-local hashmaps + merge", || {
        let locals: Vec<HashMap<u32, (i64, u64)>> = uid
            .par_chunks(CHUNK)
            .zip(amount.par_chunks(CHUNK))
            .fold(
                || HashMap::with_capacity(1 << 16),
                |mut m: HashMap<u32, (i64, u64)>, (uu, vv)| {
                    for (&u, &v) in uu.iter().zip(vv) {
                        let e = m.entry(u).or_insert((0, 0));
                        e.0 += v;
                        e.1 += 1;
                    }
                    m
                },
            )
            .collect();
        let mut m: HashMap<u32, (i64, u64)> = HashMap::with_capacity(n * 2);
        for local in locals {
            for (k, (s, c)) in local {
                let e = m.entry(k).or_insert((0, 0));
                e.0 += s;
                e.1 += c;
            }
        }
        let mut ck = 0u64;
        for (k, (s, c)) in &m {
            ck = ck.wrapping_add(ck_entry(*k as u64, *s, *c));
        }
        ck
    }));

    check_consistency(&rs);
}
