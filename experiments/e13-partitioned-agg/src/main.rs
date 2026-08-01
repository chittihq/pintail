//! e13: High-cardinality parallel aggregation — partitioned shards vs the
//! contenders that already lost or won elsewhere.
//!
//! Contested question (born from the Q6 regression, commit e5ba3ca): the
//! buffered thread-local-hashmap-plus-merge path collapsed at 2M sparse u64
//! groups (per-row key allocation, per-round global merges). e02 only
//! measured up to 200k groups. What actually wins at 200k / 2M / 8M sparse
//! keys over 20M rows?
//!
//! Variants (identical checksums required):
//!  - sequential hashmap (the engine's direct path today)
//!  - thread-local hashmaps + final merge (the shape that regressed)
//!  - hash-partitioned shards: each worker owns partition p where
//!    hash(key) % P == p; workers scan all chunks but only accumulate their
//!    own partition — no cross-thread merge at all
//!  - two-pass partitioned: pass 1 scatters (key, value) into P per-thread
//!    buckets, pass 2 aggregates each bucket independently

use common::*;
use hashbrown::HashMap;
use rayon::prelude::*;

const ROWS: usize = 20_000_000;
const CHUNK: usize = 1 << 16; // 64k morsels per e10's verdict

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn dataset(cardinality: u64) -> (Vec<u64>, Vec<i64>) {
    let mut rng = Lcg::new(0xe13 + cardinality);
    let keys = (0..ROWS)
        // Sparse keys: spread over a 100x larger id space, like user_ids.
        .map(|_| rng.below(cardinality) * 97 + 13)
        .collect::<Vec<_>>();
    let values = (0..ROWS).map(|i| (i % 1000) as i64).collect::<Vec<_>>();
    (keys, values)
}

fn checksum_groups(groups: impl Iterator<Item = (u64, i64, u64)>) -> u64 {
    groups.fold(0u64, |acc, (key, sum, count)| {
        acc ^ (key + 1).wrapping_mul((sum as u64) ^ count)
    })
}

fn sequential(keys: &[u64], values: &[i64]) -> u64 {
    let mut groups: HashMap<u64, (i64, u64)> = HashMap::new();
    for (key, value) in keys.iter().zip(values) {
        let entry = groups.entry(*key).or_insert((0, 0));
        entry.0 += value;
        entry.1 += 1;
    }
    checksum_groups(groups.into_iter().map(|(k, (s, c))| (k, s, c)))
}

fn local_maps_merge(keys: &[u64], values: &[i64]) -> u64 {
    let locals: Vec<HashMap<u64, (i64, u64)>> = keys
        .par_chunks(CHUNK)
        .zip(values.par_chunks(CHUNK))
        .map(|(keys, values)| {
            let mut local: HashMap<u64, (i64, u64)> = HashMap::new();
            for (key, value) in keys.iter().zip(values) {
                let entry = local.entry(*key).or_insert((0, 0));
                entry.0 += value;
                entry.1 += 1;
            }
            local
        })
        .collect();
    let mut global: HashMap<u64, (i64, u64)> = HashMap::new();
    for local in locals {
        for (key, (sum, count)) in local {
            let entry = global.entry(key).or_insert((0, 0));
            entry.0 += sum;
            entry.1 += count;
        }
    }
    checksum_groups(global.into_iter().map(|(k, (s, c))| (k, s, c)))
}

fn partitioned_shards(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    let shards: Vec<HashMap<u64, (i64, u64)>> = (0..partitions)
        .into_par_iter()
        .map(|partition| {
            let mut shard: HashMap<u64, (i64, u64)> = HashMap::new();
            for (key, value) in keys.iter().zip(values) {
                if (mix64(*key) as usize) % partitions == partition {
                    let entry = shard.entry(*key).or_insert((0, 0));
                    entry.0 += value;
                    entry.1 += 1;
                }
            }
            shard
        })
        .collect();
    shards
        .into_iter()
        .map(|shard| checksum_groups(shard.into_iter().map(|(k, (s, c))| (k, s, c))))
        .fold(0, |acc, c| acc ^ c)
}

fn two_pass_partitioned(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    // Pass 1: every worker scatters its chunk into P private buckets.
    let scattered: Vec<Vec<Vec<(u64, i64)>>> = keys
        .par_chunks(CHUNK)
        .zip(values.par_chunks(CHUNK))
        .map(|(keys, values)| {
            let mut buckets: Vec<Vec<(u64, i64)>> =
                (0..partitions).map(|_| Vec::new()).collect();
            for (key, value) in keys.iter().zip(values) {
                buckets[(mix64(*key) as usize) % partitions].push((*key, *value));
            }
            buckets
        })
        .collect();
    // Pass 2: each partition aggregates its buckets from every worker.
    (0..partitions)
        .into_par_iter()
        .map(|partition| {
            let mut shard: HashMap<u64, (i64, u64)> = HashMap::new();
            for chunk_buckets in &scattered {
                for (key, value) in &chunk_buckets[partition] {
                    let entry = shard.entry(*key).or_insert((0, 0));
                    entry.0 += value;
                    entry.1 += 1;
                }
            }
            checksum_groups(shard.drain().map(|(k, (s, c))| (k, s, c)))
        })
        .reduce(|| 0, |a, b| a ^ b)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!(
        "e13: {} rows, {} threads, chunk {}",
        ROWS, threads, CHUNK
    );
    for cardinality in [200_000u64, 2_000_000, 8_000_000] {
        println!("== cardinality {cardinality} ==");
        let (keys, values) = dataset(cardinality);
        let results = [
            bench("sequential hashmap", || sequential(&keys, &values)),
            bench("thread-local maps + merge", || {
                local_maps_merge(&keys, &values)
            }),
            bench("partitioned shards (P=threads)", || {
                partitioned_shards(&keys, &values, threads)
            }),
            bench("two-pass partitioned (P=threads)", || {
                two_pass_partitioned(&keys, &values, threads)
            }),
        ];
        let first = results[0].checksum;
        assert!(
            results.iter().all(|result| result.checksum == first),
            "checksum mismatch: comparison void"
        );
    }
}
