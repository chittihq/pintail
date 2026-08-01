//! e14: Typed aggregation kernels vs the Value-enum loop on the Q5 shape.
//!
//! Contested question (born from the 63becb4 benchmark run): Q5 groups 20M
//! rows into only ~24 (year, month) buckets yet takes 5.6s — 3x longer than
//! Q3's 5-group group-by and 26x behind ClickHouse. The suspect is not the
//! storage layer but the execution path: a two-column GROUP BY is ineligible
//! for the single-int-column two-pass path, so every row allocates a
//! Vec<Value> key, hashes it, and dispatches aggregate updates through enum
//! matches. What is actually on the table if the hot loop goes typed?
//!
//! Variants (identical checksums required; all include the same per-row
//! date->(year, month) conversion and the same year==2023 filter, mirroring
//! Q5's WHERE + expression work):
//!  - value-enum rows: Vec<Value> key into HashMap, enum-dispatched updates
//!    (the engine's sequential multi-column path today)
//!  - typed composite key: (year, month) packed into one u64, typed hashmap
//!    (what extending two-pass eligibility to composite int keys buys,
//!    single-threaded floor)
//!  - typed dense array: perfect index into a flat 36-slot accumulator, no
//!    hashing at all (the ClickHouse-style low-cardinality specialization)
//!  - two-pass partitioned, composite u64 key (e13's winner generalized to
//!    composite keys, P=threads)
//!  - dense array per worker + merge: flat accumulators per rayon chunk,
//!    36-slot merge (the parallel ceiling for this shape)

use common::*;
use hashbrown::HashMap;
use rayon::prelude::*;

const CHUNK: usize = 1 << 16; // 64k morsels per e10's verdict

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// civil_from_days (Howard Hinnant): days since 1970-01-01 -> (year, month).
#[inline]
fn year_month(days: i32) -> (i32, u32) {
    let z = i64::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    ((y + i64::from(m <= 2)) as i32, m as u32)
}

const FILTER_YEAR: i32 = 2023;

fn checksum_groups(groups: impl Iterator<Item = (u64, i64, u64)>) -> u64 {
    groups.fold(0u64, |acc, (key, sum, count)| {
        acc ^ (key + 1).wrapping_mul((sum as u64) ^ count)
    })
}

/// The engine's Value enum, reduced to the variants this query touches.
/// Vec<Value> keys are allocated per row exactly like the sequential
/// multi-column group path does.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Value {
    Int64(i64),
    UInt64(u64),
}

fn value_enum_rows(dates: &[i32], amounts: &[i64]) -> u64 {
    let mut groups: HashMap<Vec<Value>, (i64, u64)> = HashMap::new();
    for (date, amount) in dates.iter().zip(amounts) {
        let (year, month) = year_month(*date);
        if year != FILTER_YEAR {
            continue;
        }
        let key = vec![
            Value::Int64(i64::from(year)),
            Value::UInt64(u64::from(month)),
        ];
        let entry = groups.entry(key).or_insert((0, 0));
        // Enum-dispatched update, like AggregateState::update on a &Value.
        match Value::Int64(*amount) {
            Value::Int64(v) => entry.0 += v,
            Value::UInt64(v) => entry.0 += v as i64,
        }
        entry.1 += 1;
    }
    checksum_groups(groups.into_iter().map(|(key, (sum, count))| {
        let packed = match (&key[0], &key[1]) {
            (Value::Int64(y), Value::UInt64(m)) => pack(*y as i32, *m as u32),
            _ => unreachable!(),
        };
        (packed, sum, count)
    }))
}

#[inline]
#[allow(clippy::cast_sign_loss)]
fn pack(year: i32, month: u32) -> u64 {
    ((year as u64) << 8) | u64::from(month)
}

fn typed_composite_key(dates: &[i32], amounts: &[i64]) -> u64 {
    let mut groups: HashMap<u64, (i64, u64)> = HashMap::new();
    for (date, amount) in dates.iter().zip(amounts) {
        let (year, month) = year_month(*date);
        if year != FILTER_YEAR {
            continue;
        }
        let entry = groups.entry(pack(year, month)).or_insert((0, 0));
        entry.0 += amount;
        entry.1 += 1;
    }
    checksum_groups(groups.into_iter().map(|(k, (s, c))| (k, s, c)))
}

/// 3 years x 12 months of dense slots; index = (year - 2022) * 12 + month-1.
const SLOTS: usize = 36;

#[inline]
fn slot(year: i32, month: u32) -> usize {
    #[allow(clippy::cast_sign_loss)]
    {
        ((year - 2022) as usize) * 12 + (month as usize - 1)
    }
}

fn dense_checksum(slots: &[(i64, u64); SLOTS]) -> u64 {
    checksum_groups(slots.iter().enumerate().filter_map(|(i, (s, c))| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        (*c > 0).then(|| (pack(2022 + (i / 12) as i32, (i % 12) as u32 + 1), *s, *c))
    }))
}

fn typed_dense_array(dates: &[i32], amounts: &[i64]) -> u64 {
    let mut slots = [(0i64, 0u64); SLOTS];
    for (date, amount) in dates.iter().zip(amounts) {
        let (year, month) = year_month(*date);
        if year != FILTER_YEAR {
            continue;
        }
        let entry = &mut slots[slot(year, month)];
        entry.0 += amount;
        entry.1 += 1;
    }
    dense_checksum(&slots)
}

fn two_pass_composite(dates: &[i32], amounts: &[i64], partitions: usize) -> u64 {
    let scattered: Vec<Vec<Vec<(u64, i64)>>> = dates
        .par_chunks(CHUNK)
        .zip(amounts.par_chunks(CHUNK))
        .map(|(dates, amounts)| {
            let mut buckets: Vec<Vec<(u64, i64)>> = (0..partitions).map(|_| Vec::new()).collect();
            for (date, amount) in dates.iter().zip(amounts) {
                let (year, month) = year_month(*date);
                if year != FILTER_YEAR {
                    continue;
                }
                let key = pack(year, month);
                buckets[(mix64(key) as usize) % partitions].push((key, *amount));
            }
            buckets
        })
        .collect();
    (0..partitions)
        .into_par_iter()
        .map(|partition| {
            let mut shard: HashMap<u64, (i64, u64)> = HashMap::new();
            for chunk_buckets in &scattered {
                for (key, amount) in &chunk_buckets[partition] {
                    let entry = shard.entry(*key).or_insert((0, 0));
                    entry.0 += amount;
                    entry.1 += 1;
                }
            }
            checksum_groups(shard.drain().map(|(k, (s, c))| (k, s, c)))
        })
        .reduce(|| 0, |a, b| a ^ b)
}

fn dense_array_parallel(dates: &[i32], amounts: &[i64]) -> u64 {
    let locals: Vec<[(i64, u64); SLOTS]> = dates
        .par_chunks(CHUNK)
        .zip(amounts.par_chunks(CHUNK))
        .map(|(dates, amounts)| {
            let mut slots = [(0i64, 0u64); SLOTS];
            for (date, amount) in dates.iter().zip(amounts) {
                let (year, month) = year_month(*date);
                if year != FILTER_YEAR {
                    continue;
                }
                let entry = &mut slots[slot(year, month)];
                entry.0 += amount;
                entry.1 += 1;
            }
            slots
        })
        .collect();
    let mut merged = [(0i64, 0u64); SLOTS];
    for local in locals {
        for (into, from) in merged.iter_mut().zip(local) {
            into.0 += from.0;
            into.1 += from.1;
        }
    }
    dense_checksum(&merged)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e14: {N_ORDERS} rows, {threads} threads, chunk {CHUNK}");
    let orders = gen_orders(N_ORDERS, 0xe14);
    let (dates, amounts) = (&orders.date, &orders.amount);
    let results = [
        bench("value-enum rows (engine multi-column path)", || {
            value_enum_rows(dates, amounts)
        }),
        bench("typed composite u64 key + hashmap", || {
            typed_composite_key(dates, amounts)
        }),
        bench("typed dense-array kernel", || {
            typed_dense_array(dates, amounts)
        }),
        bench("two-pass partitioned (composite key)", || {
            two_pass_composite(dates, amounts, threads)
        }),
        bench("dense array per worker + merge", || {
            dense_array_parallel(dates, amounts)
        }),
    ];
    check_consistency(&results);
}
