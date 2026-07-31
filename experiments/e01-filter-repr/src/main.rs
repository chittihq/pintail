//! e01: Filter representation shoot-out.
//!
//! Contested question: Photon says position lists beat byte masks; ClickHouse's
//! source is byte-mask + popcount everywhere; DaMoN 2021 says "it depends on
//! selectivity". Which is true on our data shapes?
//!
//! Variants answer the same two query shapes at several selectivities:
//!   COUNT(*)    WHERE amount > t
//!   SUM(amount) WHERE amount > t
//!   SUM(amount) WHERE status = 2       (dict-code predicate, 20% cyclic)

use common::*;

fn main() {
    println!("e01-filter-repr  N = {N_ORDERS}");
    let o = gen_orders(N_ORDERS, 42);
    let amount = &o.amount;
    let status = &o.status;

    let mut mask = vec![0u8; amount.len()];
    let mut words = vec![0u64; amount.len().div_ceil(64)];
    let mut sel: Vec<u32> = Vec::with_capacity(amount.len());

    for sel_pct in [1u64, 10, 50, 90] {
        let span = 999_900u64;
        let t = (100 + span - span * sel_pct / 100) as i64;

        println!("\n== COUNT(*) WHERE amount > {t}  (~{sel_pct}%) ==");
        let mut rs = vec![];
        rs.push(bench("fused: iter().filter().count()", || {
            amount.iter().filter(|&&v| v > t).count() as u64
        }));
        rs.push(bench("fused: branchless predicate sum", || {
            let mut c = 0u64;
            for &v in amount {
                c += (v > t) as u64;
            }
            c
        }));
        rs.push(bench("byte mask: build + popcount pass", || {
            for (m, &v) in mask.iter_mut().zip(amount) {
                *m = (v > t) as u8;
            }
            mask.iter().map(|&m| m as u64).sum()
        }));
        rs.push(bench("bitmap: build words + count_ones", || {
            for (w, chunk) in words.iter_mut().zip(amount.chunks(64)) {
                let mut word = 0u64;
                for (b, &v) in chunk.iter().enumerate() {
                    word |= ((v > t) as u64) << b;
                }
                *w = word;
            }
            words.iter().map(|w| w.count_ones() as u64).sum()
        }));
        rs.push(bench("selection vector: build + len", || {
            sel.clear();
            for (i, &v) in amount.iter().enumerate() {
                if v > t {
                    sel.push(i as u32);
                }
            }
            sel.len() as u64
        }));
        check_consistency(&rs);

        println!("== SUM(amount) WHERE amount > {t}  (~{sel_pct}%) ==");
        let mut rs = vec![];
        rs.push(bench("fused: branchless multiply-sum", || {
            let mut s = 0i64;
            for &v in amount {
                s += ((v > t) as i64) * v;
            }
            s as u64
        }));
        rs.push(bench("fused: branchy if-sum", || {
            let mut s = 0i64;
            for &v in amount {
                if v > t {
                    s += v;
                }
            }
            s as u64
        }));
        rs.push(bench("byte mask: build, then masked sum", || {
            for (m, &v) in mask.iter_mut().zip(amount) {
                *m = (v > t) as u8;
            }
            let mut s = 0i64;
            for (&m, &v) in mask.iter().zip(amount) {
                s += (m as i64) * v;
            }
            s as u64
        }));
        rs.push(bench("bitmap: build, then iterate set bits", || {
            for (w, chunk) in words.iter_mut().zip(amount.chunks(64)) {
                let mut word = 0u64;
                for (b, &v) in chunk.iter().enumerate() {
                    word |= ((v > t) as u64) << b;
                }
                *w = word;
            }
            let mut s = 0i64;
            for (wi, &w) in words.iter().enumerate() {
                let mut w = w;
                while w != 0 {
                    let b = w.trailing_zeros() as usize;
                    s += amount[wi * 64 + b];
                    w &= w - 1;
                }
            }
            s as u64
        }));
        rs.push(bench("selection vector: build, then gather-sum", || {
            sel.clear();
            for (i, &v) in amount.iter().enumerate() {
                if v > t {
                    sel.push(i as u32);
                }
            }
            let mut s = 0i64;
            for &i in &sel {
                s += amount[i as usize];
            }
            s as u64
        }));
        check_consistency(&rs);
    }

    println!("\n== SUM(amount) WHERE status = 2  (dict predicate, 20% cyclic — the Q2 shape) ==");
    let mut rs = vec![];
    rs.push(bench("fused: branchless over two columns", || {
        let mut s = 0i64;
        for (&st, &v) in status.iter().zip(amount) {
            s += ((st == 2) as i64) * v;
        }
        s as u64
    }));
    rs.push(bench("byte mask from status, masked sum amount", || {
        for (m, &st) in mask.iter_mut().zip(status) {
            *m = (st == 2) as u8;
        }
        let mut s = 0i64;
        for (&m, &v) in mask.iter().zip(amount) {
            s += (m as i64) * v;
        }
        s as u64
    }));
    rs.push(bench("bitmap from status, iterate set bits", || {
        for (w, chunk) in words.iter_mut().zip(status.chunks(64)) {
            let mut word = 0u64;
            for (b, &st) in chunk.iter().enumerate() {
                word |= ((st == 2) as u64) << b;
            }
            *w = word;
        }
        let mut s = 0i64;
        for (wi, &w) in words.iter().enumerate() {
            let mut w = w;
            while w != 0 {
                let b = w.trailing_zeros() as usize;
                s += amount[wi * 64 + b];
                w &= w - 1;
            }
        }
        s as u64
    }));
    rs.push(bench("selection vector from status, gather amount", || {
        sel.clear();
        for (i, &st) in status.iter().enumerate() {
            if st == 2 {
                sel.push(i as u32);
            }
        }
        let mut s = 0i64;
        for &i in &sel {
            s += amount[i as usize];
        }
        s as u64
    }));
    check_consistency(&rs);
}
