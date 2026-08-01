//! e16: COUNT(DISTINCT user_id) per region — the Q7 lane with no lab budget.
//!
//! Contested question (born from the 63becb4 run): Q7 runs 6 aggregates
//! including COUNT(DISTINCT user_id) over 8 region groups and sits 31x
//! behind ClickHouse. The engine keeps a HashSet of Value cells per group.
//! e04 proved dense bitmaps beat hash sets for semi-join membership; does
//! the same hold for grouped distinct-count, and what is the parallel
//! shape — per-worker bitmaps OR-merged, or partitioned sets?
//!
//! Variants (identical checksums; 20M rows, 8 regions, 200k user space,
//! COUNT(DISTINCT user) + SUM(amount) per region):
//!  - HashSet<Value> per group (the engine's DISTINCT path today)
//!  - HashSet<u32> per group (typed hashing, no enum cells)
//!  - dense bitmap per group: 8 x 200k bits = 200 KB, popcount at the end
//!  - parallel per-worker bitmaps + OR-merge (the e04 shape, grouped)
//!  - parallel partitioned by user: each worker owns a user range, no merge

use common::*;
use hashbrown::HashSet;
use rayon::prelude::*;

const CHUNK: usize = 1 << 16;
const WORDS_PER_GROUP: usize = (N_USERS as usize).div_ceil(64);

/// The engine's Value enum (real variant set, honest cell size + dispatch).
#[derive(Clone, PartialEq, Eq, Hash)]
enum Value {
    #[allow(dead_code)]
    Null,
    #[allow(dead_code)]
    Boolean(bool),
    #[allow(dead_code)]
    Int64(i64),
    UInt64(u64),
    #[allow(dead_code)]
    Utf8(String),
}

fn checksum_groups(groups: impl Iterator<Item = (usize, u64, i64)>) -> u64 {
    groups.fold(0u64, |acc, (region, distinct, sum)| {
        acc ^ ((region as u64) + 1).wrapping_mul(distinct ^ (sum as u64))
    })
}

fn value_hash_sets(orders: &Orders) -> u64 {
    let mut seen: Vec<HashSet<Value>> = (0..N_REGIONS).map(|_| HashSet::new()).collect();
    let mut sums = [0i64; N_REGIONS];
    for ((region, user), amount) in orders
        .region
        .iter()
        .zip(&orders.user_id)
        .zip(&orders.amount)
    {
        seen[*region as usize].insert(Value::UInt64(u64::from(*user)));
        sums[*region as usize] += amount;
    }
    checksum_groups(
        seen.iter()
            .enumerate()
            .map(|(region, set)| (region, set.len() as u64, sums[region])),
    )
}

fn typed_hash_sets(orders: &Orders) -> u64 {
    let mut seen: Vec<HashSet<u32>> = (0..N_REGIONS).map(|_| HashSet::new()).collect();
    let mut sums = [0i64; N_REGIONS];
    for ((region, user), amount) in orders
        .region
        .iter()
        .zip(&orders.user_id)
        .zip(&orders.amount)
    {
        seen[*region as usize].insert(*user);
        sums[*region as usize] += amount;
    }
    checksum_groups(
        seen.iter()
            .enumerate()
            .map(|(region, set)| (region, set.len() as u64, sums[region])),
    )
}

fn count_bits(bitmap: &[u64]) -> u64 {
    bitmap.iter().map(|word| u64::from(word.count_ones())).sum()
}

fn dense_bitmaps(orders: &Orders) -> u64 {
    let mut bits = vec![0u64; N_REGIONS * WORDS_PER_GROUP];
    let mut sums = [0i64; N_REGIONS];
    for ((region, user), amount) in orders
        .region
        .iter()
        .zip(&orders.user_id)
        .zip(&orders.amount)
    {
        let user = *user as usize;
        bits[*region as usize * WORDS_PER_GROUP + user / 64] |= 1 << (user % 64);
        sums[*region as usize] += amount;
    }
    checksum_groups((0..N_REGIONS).map(|region| {
        (
            region,
            count_bits(&bits[region * WORDS_PER_GROUP..(region + 1) * WORDS_PER_GROUP]),
            sums[region],
        )
    }))
}

fn parallel_bitmaps_merge(orders: &Orders) -> u64 {
    let locals: Vec<(Vec<u64>, [i64; N_REGIONS])> = orders
        .region
        .par_chunks(CHUNK)
        .zip(orders.user_id.par_chunks(CHUNK))
        .zip(orders.amount.par_chunks(CHUNK))
        .map(|((regions, users), amounts)| {
            let mut bits = vec![0u64; N_REGIONS * WORDS_PER_GROUP];
            let mut sums = [0i64; N_REGIONS];
            for ((region, user), amount) in regions.iter().zip(users).zip(amounts) {
                let user = *user as usize;
                bits[*region as usize * WORDS_PER_GROUP + user / 64] |= 1 << (user % 64);
                sums[*region as usize] += amount;
            }
            (bits, sums)
        })
        .collect();
    let mut bits = vec![0u64; N_REGIONS * WORDS_PER_GROUP];
    let mut sums = [0i64; N_REGIONS];
    for (local_bits, local_sums) in locals {
        for (into, from) in bits.iter_mut().zip(local_bits) {
            *into |= from;
        }
        for (into, from) in sums.iter_mut().zip(local_sums) {
            *into += from;
        }
    }
    checksum_groups((0..N_REGIONS).map(|region| {
        (
            region,
            count_bits(&bits[region * WORDS_PER_GROUP..(region + 1) * WORDS_PER_GROUP]),
            sums[region],
        )
    }))
}

fn parallel_user_partitioned(orders: &Orders, partitions: usize) -> u64 {
    // Each worker owns a user-id range; every worker scans all rows but
    // only records its own users. No merge: distinct counts add across
    // disjoint ranges. Sums accumulate on partition 0 only.
    let span = (N_USERS as usize).div_ceil(partitions);
    let parts: Vec<([u64; N_REGIONS], [i64; N_REGIONS])> = (0..partitions)
        .into_par_iter()
        .map(|partition| {
            let low = partition * span;
            let high = ((partition + 1) * span).min(N_USERS as usize);
            let words = (high - low).div_ceil(64);
            let mut bits = vec![0u64; N_REGIONS * words];
            let mut sums = [0i64; N_REGIONS];
            for ((region, user), amount) in orders
                .region
                .iter()
                .zip(&orders.user_id)
                .zip(&orders.amount)
            {
                let user = *user as usize;
                if user >= low && user < high {
                    bits[*region as usize * words + (user - low) / 64] |= 1 << ((user - low) % 64);
                }
                if partition == 0 {
                    sums[*region as usize] += amount;
                }
            }
            let mut distinct = [0u64; N_REGIONS];
            for (region, slot) in distinct.iter_mut().enumerate() {
                *slot = count_bits(&bits[region * words..(region + 1) * words]);
            }
            (distinct, sums)
        })
        .collect();
    let mut distinct = [0u64; N_REGIONS];
    let mut sums = [0i64; N_REGIONS];
    for (part_distinct, part_sums) in parts {
        for (into, from) in distinct.iter_mut().zip(part_distinct) {
            *into += from;
        }
        for (into, from) in sums.iter_mut().zip(part_sums) {
            *into += from;
        }
    }
    checksum_groups((0..N_REGIONS).map(|region| (region, distinct[region], sums[region])))
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e16: {N_ORDERS} rows, {N_REGIONS} regions, {N_USERS} user space, {threads} threads");
    let orders = gen_orders(N_ORDERS, 0xe16);
    let results = [
        bench("HashSet<Value> per group (engine today)", || {
            value_hash_sets(&orders)
        }),
        bench("HashSet<u32> per group", || typed_hash_sets(&orders)),
        bench("dense bitmap per group (200 KB)", || dense_bitmaps(&orders)),
        bench("parallel bitmaps + OR-merge", || {
            parallel_bitmaps_merge(&orders)
        }),
        bench("parallel user-partitioned bitmaps", || {
            parallel_user_partitioned(&orders, threads)
        }),
    ];
    check_consistency(&results);
}
