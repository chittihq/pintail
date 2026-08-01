//! e19: Executing directly on compressed data (FOR + bit-packing).
//!
//! Paper: Abadi, Madden, Ferreira — "Integrating Compression and
//! Execution in Column-Oriented Database Systems" (SIGMOD 2006): the
//! executor should consume compressed representations, not decompress
//! into a neutral format first. BtrBlocks (SIGMOD 2023) and FastLanes
//! (VLDB) push the same idea to SIMD-width encodings.
//!
//! Contested question: PTSEG stores integers/decimals as native i64
//! units; a v3 encoding pass will want frame-of-reference + bit-packing
//! (e06 measured decode cost alone). When the consumer is an aggregate,
//! does pintail ever need the decoded array at all? FOR algebra gives
//! SUM = count*base + SUM(deltas), and the delta sum can accumulate
//! during unpacking with no scratch write.
//!
//! Variants (identical checksums; 20M rows, amount i64 packed FOR+bitpack
//! per 64k block, status u8 dict codes kept raw; parallel per block):
//!  - unpack to scratch, then fused SUM (decode-then-kernel reference)
//!  - fused unpack-accumulate: SUM(deltas) during unpack + base algebra,
//!    no scratch materialization
//!  - filtered: unpack to scratch, then e01 fused filter+SUM on codes
//!  - filtered fused: single pass over packed words + codes, branchless
//!    accumulate, no scratch

use common::*;
use rayon::prelude::*;

const ROWS: usize = N_ORDERS;
const BLOCK: usize = 1 << 16;
const WANTED_STATUS: u8 = 2;

struct PackedBlock {
    rows: usize,
    base: i64,
    width: u32,
    words: Vec<u64>,
}

fn pack_blocks(amounts: &[i64]) -> Vec<PackedBlock> {
    amounts
        .chunks(BLOCK)
        .map(|amounts| {
            let base = amounts.iter().copied().min().expect("non-empty block");
            let range = amounts.iter().copied().max().expect("non-empty block") - base;
            #[allow(clippy::cast_sign_loss)]
            let width = 64 - (range as u64).leading_zeros();
            let mut words = vec![0u64; (amounts.len() * width as usize).div_ceil(64)];
            for (row, amount) in amounts.iter().enumerate() {
                #[allow(clippy::cast_sign_loss)]
                let delta = (amount - base) as u64;
                let bit = row * width as usize;
                words[bit / 64] |= delta << (bit % 64);
                if bit % 64 + width as usize > 64 {
                    words[bit / 64 + 1] |= delta >> (64 - bit % 64);
                }
            }
            PackedBlock {
                rows: amounts.len(),
                base,
                width,
                words,
            }
        })
        .collect()
}

#[inline]
fn unpack_at(block: &PackedBlock, row: usize) -> u64 {
    let width = block.width as usize;
    let bit = row * width;
    let mask = if width == 64 { u64::MAX } else { (1 << width) - 1 };
    let mut delta = block.words[bit / 64] >> (bit % 64);
    if bit % 64 + width > 64 {
        delta |= block.words[bit / 64 + 1] << (64 - bit % 64);
    }
    delta & mask
}

fn unpack_block(block: &PackedBlock, scratch: &mut Vec<i64>) {
    scratch.clear();
    scratch.extend((0..block.rows).map(|row| block.base + unpack_at(block, row) as i64));
}

fn checksum(sum: i64, count: u64) -> u64 {
    (sum as u64) ^ count.rotate_left(32)
}

fn global_scratch(blocks: &[PackedBlock]) -> u64 {
    let sum = blocks
        .par_iter()
        .map_init(
            || Vec::with_capacity(BLOCK),
            |scratch, block| {
                unpack_block(block, scratch);
                scratch.iter().sum::<i64>()
            },
        )
        .sum::<i64>();
    checksum(sum, ROWS as u64)
}

fn global_fused(blocks: &[PackedBlock]) -> u64 {
    let sum = blocks
        .par_iter()
        .map(|block| {
            let deltas: u64 = (0..block.rows).map(|row| unpack_at(block, row)).sum();
            block.base * block.rows as i64 + deltas as i64
        })
        .sum::<i64>();
    checksum(sum, ROWS as u64)
}

fn filtered_scratch(blocks: &[PackedBlock], status: &[u8]) -> u64 {
    let (sum, count) = blocks
        .par_iter()
        .enumerate()
        .map_init(
            || Vec::with_capacity(BLOCK),
            |scratch, (index, block)| {
                unpack_block(block, scratch);
                let codes = &status[index * BLOCK..index * BLOCK + block.rows];
                let mut sum = 0i64;
                let mut count = 0u64;
                for (code, amount) in codes.iter().zip(scratch.iter()) {
                    let hit = i64::from(*code == WANTED_STATUS);
                    sum += hit * amount;
                    count += hit as u64;
                }
                (sum, count)
            },
        )
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    checksum(sum, count)
}

fn filtered_fused(blocks: &[PackedBlock], status: &[u8]) -> u64 {
    let (sum, count) = blocks
        .par_iter()
        .enumerate()
        .map(|(index, block)| {
            let codes = &status[index * BLOCK..index * BLOCK + block.rows];
            let mut delta_sum = 0u64;
            let mut count = 0u64;
            for (row, code) in codes.iter().enumerate() {
                let hit = u64::from(*code == WANTED_STATUS);
                delta_sum += hit * unpack_at(block, row);
                count += hit;
            }
            #[allow(clippy::cast_possible_wrap)]
            (block.base * count as i64 + delta_sum as i64, count)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    checksum(sum, count)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e19: {ROWS} rows, {BLOCK}-row FOR+bitpack blocks, {threads} threads");
    let orders = gen_orders(ROWS, 0xe19);
    let blocks = pack_blocks(&orders.amount);
    let packed: usize = blocks.iter().map(|block| block.words.len() * 8).sum();
    println!(
        "packed width {} bits, {:.1} MB (raw {:.1} MB)",
        blocks[0].width,
        packed as f64 / 1e6,
        (ROWS * 8) as f64 / 1e6
    );
    let global = [
        bench("global SUM: unpack to scratch, then sum", || {
            global_scratch(&blocks)
        }),
        bench("global SUM: fused unpack-accumulate + FOR algebra", || {
            global_fused(&blocks)
        }),
    ];
    check_consistency(&global);
    let filtered = [
        bench("filtered SUM: unpack to scratch, then fused", || {
            filtered_scratch(&blocks, &orders.status)
        }),
        bench("filtered SUM: single pass on packed + codes", || {
            filtered_fused(&blocks, &orders.status)
        }),
    ];
    check_consistency(&filtered);
}
