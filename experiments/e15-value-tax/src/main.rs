//! e15: The Value-enum middle layer's tax on the Q6 shape.
//!
//! Contested question (born from e13/e14 vs the 63becb4 run): every lab
//! kernel budget assumes typed arrays arriving at the kernel — e13's
//! two-pass runs 2M sparse groups in ~47 ms, yet the engine's Q6 takes
//! 11.7 s with that very kernel adopted. The claim "the Value
//! materialization between storage and kernel is the bottleneck" is
//! load-bearing for the whole typed-kernel program and has never been
//! measured in isolation. What do enum cells actually cost end-to-end?
//!
//! Variants (identical checksums; 20M rows, 2M sparse u64 keys,
//! SUM(i64)+COUNT per group, two-pass partitioned aggregation P=threads
//! everywhere except the sequential engine model):
//!  - typed contiguous arrays (e13's reference: the ceiling)
//!  - typed 64k-chunk batches (chunk-boundary tax only)
//!  - Value-enum column batches, kernel matches per cell (the engine
//!    today: DecodedColumn materializes Vec<Value>, two_pass_key_bits and
//!    the lane loop match on &Value per row)
//!  - Value-enum batches built row-major then transposed to columns
//!    (models the row-path adopt_chunk shape for CDC-fed segments)
//!  - Value-enum batches + sequential enum-dispatch hashmap (the engine's
//!    pre-two-pass sequential path, for scale)

use common::*;
use hashbrown::HashMap;
use rayon::prelude::*;

const ROWS: usize = 20_000_000;
const CARDINALITY: u64 = 2_000_000;
const CHUNK: usize = 1 << 16;

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The engine's Value enum with its real variant set, so cell size (32
/// bytes: discriminant + String payload) and match dispatch are honest.
#[derive(Clone)]
enum Value {
    #[allow(dead_code)]
    Null,
    #[allow(dead_code)]
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    #[allow(dead_code)]
    Float64(f64),
    #[allow(dead_code)]
    Utf8(String),
    #[allow(dead_code)]
    Binary(Vec<u8>),
}

fn dataset() -> (Vec<u64>, Vec<i64>) {
    let mut rng = Lcg::new(0xe15);
    let keys = (0..ROWS)
        .map(|_| rng.below(CARDINALITY) * 97 + 13)
        .collect::<Vec<_>>();
    let values = (0..ROWS).map(|i| (i % 1000) as i64).collect::<Vec<_>>();
    (keys, values)
}

fn checksum_groups(groups: impl Iterator<Item = (u64, i64, u64)>) -> u64 {
    groups.fold(0u64, |acc, (key, sum, count)| {
        acc ^ (key + 1).wrapping_mul((sum as u64) ^ count)
    })
}

/// Two-pass partitioned aggregation over already-typed (u64, i64) pairs.
fn two_pass<'a, I, C>(chunks: I, partitions: usize) -> u64
where
    I: IntoParallelIterator<Item = C>,
    C: Iterator<Item = (u64, i64)> + Send,
{
    let scattered: Vec<Vec<Vec<(u64, i64)>>> = chunks
        .into_par_iter()
        .map(|chunk| {
            let mut buckets: Vec<Vec<(u64, i64)>> = (0..partitions).map(|_| Vec::new()).collect();
            for (key, value) in chunk {
                buckets[(mix64(key) as usize) % partitions].push((key, value));
            }
            buckets
        })
        .collect();
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

fn typed_arrays(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    two_pass(
        keys.par_chunks(CHUNK)
            .zip(values.par_chunks(CHUNK))
            .map(|(keys, values)| keys.iter().copied().zip(values.iter().copied()))
            .collect::<Vec<_>>(),
        partitions,
    )
}

struct TypedBatch {
    keys: Vec<u64>,
    values: Vec<i64>,
}

fn typed_batches(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    // Materialize batch structs first (the chunk-boundary + copy tax),
    // then aggregate. Build cost is measured; it is part of the pipeline.
    let batches: Vec<TypedBatch> = keys
        .chunks(CHUNK)
        .zip(values.chunks(CHUNK))
        .map(|(keys, values)| TypedBatch {
            keys: keys.to_vec(),
            values: values.to_vec(),
        })
        .collect();
    two_pass(
        batches
            .par_iter()
            .map(|batch| batch.keys.iter().copied().zip(batch.values.iter().copied()))
            .collect::<Vec<_>>(),
        partitions,
    )
}

struct ValueBatch {
    keys: Vec<Value>,
    values: Vec<Value>,
}

fn build_value_batches(keys: &[u64], values: &[i64]) -> Vec<ValueBatch> {
    keys.chunks(CHUNK)
        .zip(values.chunks(CHUNK))
        .map(|(keys, values)| ValueBatch {
            keys: keys.iter().map(|key| Value::UInt64(*key)).collect(),
            values: values.iter().map(|value| Value::Int64(*value)).collect(),
        })
        .collect()
}

#[inline]
fn cell_pair(key: &Value, value: &Value) -> (u64, i64) {
    // two_pass_key_bits + the lane match, as the engine's scatter does.
    let key = match key {
        Value::UInt64(key) => *key,
        Value::Int64(key) => *key as u64,
        _ => 0,
    };
    let value = match value {
        Value::Int64(value) => *value,
        Value::UInt64(value) => *value as i64,
        _ => 0,
    };
    (key, value)
}

fn value_batches(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    let batches = build_value_batches(keys, values);
    two_pass(
        batches
            .par_iter()
            .map(|batch| {
                batch
                    .keys
                    .iter()
                    .zip(batch.values.iter())
                    .map(|(key, value)| cell_pair(key, value))
            })
            .collect::<Vec<_>>(),
        partitions,
    )
}

fn value_rows_transposed(keys: &[u64], values: &[i64], partitions: usize) -> u64 {
    // Row-major Vec<Vec<Value>> first (the CDC/adopt_chunk shape), then
    // transpose to Value columns, then aggregate.
    let batches: Vec<ValueBatch> = keys
        .chunks(CHUNK)
        .zip(values.chunks(CHUNK))
        .map(|(keys, values)| {
            let rows: Vec<Vec<Value>> = keys
                .iter()
                .zip(values)
                .map(|(key, value)| vec![Value::UInt64(*key), Value::Int64(*value)])
                .collect();
            let mut columns = ValueBatch {
                keys: Vec::with_capacity(rows.len()),
                values: Vec::with_capacity(rows.len()),
            };
            for row in rows {
                let mut row = row.into_iter();
                columns.keys.push(row.next().unwrap());
                columns.values.push(row.next().unwrap());
            }
            columns
        })
        .collect();
    two_pass(
        batches
            .par_iter()
            .map(|batch| {
                batch
                    .keys
                    .iter()
                    .zip(batch.values.iter())
                    .map(|(key, value)| cell_pair(key, value))
            })
            .collect::<Vec<_>>(),
        partitions,
    )
}

fn value_sequential(keys: &[u64], values: &[i64]) -> u64 {
    let batches = build_value_batches(keys, values);
    let mut groups: HashMap<u64, (i64, u64)> = HashMap::new();
    for batch in &batches {
        for (key, value) in batch.keys.iter().zip(batch.values.iter()) {
            let (key, value) = cell_pair(key, value);
            let entry = groups.entry(key).or_insert((0, 0));
            entry.0 += value;
            entry.1 += 1;
        }
    }
    checksum_groups(groups.into_iter().map(|(k, (s, c))| (k, s, c)))
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e15: {ROWS} rows, {CARDINALITY} sparse keys, {threads} threads, chunk {CHUNK}");
    println!("Value cell size: {} bytes", std::mem::size_of::<Value>());
    let (keys, values) = dataset();
    let results = [
        bench("typed contiguous arrays -> two-pass", || {
            typed_arrays(&keys, &values, threads)
        }),
        bench("typed 64k batches -> two-pass", || {
            typed_batches(&keys, &values, threads)
        }),
        bench("Value column batches -> two-pass", || {
            value_batches(&keys, &values, threads)
        }),
        bench("Value rows -> transpose -> two-pass", || {
            value_rows_transposed(&keys, &values, threads)
        }),
        bench("Value batches -> sequential hashmap", || {
            value_sequential(&keys, &values)
        }),
    ];
    check_consistency(&results);
}
