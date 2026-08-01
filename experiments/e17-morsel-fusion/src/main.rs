//! e17: Morsel-driven fused execution vs staged decode-then-aggregate.
//!
//! Paper: Leis, Boncz, Kemper, Neumann — "Morsel-Driven Parallelism: A
//! NUMA-Aware Query Evaluation Framework for the Many-Core Age" (SIGMOD
//! 2014). HyPer/ClickHouse-style engines run decompress→filter→partial-
//! aggregate as ONE fused pass per worker per morsel, keeping the morsel
//! hot in cache and never materializing the table.
//!
//! Contested question (decides the typed-pipeline executor structure
//! before implementation): pintail today decodes blocks into batches,
//! adopts them into a stream, and aggregates afterwards — every byte
//! crosses memory at least twice. e15 measured the enum tax; this
//! measures the STAGING tax with everything already typed, including
//! real LZ4 block decompression (PTSEG's codec).
//!
//! Variants (identical checksums; 20M rows, status u8 cycling 5 /
//! amount i64, 64k-row LZ4 blocks, SUM+COUNT WHERE status==2):
//!  - staged sequential: decode all blocks to full columns, then one
//!    fused filter+agg pass (the simplest adoption of e01's kernel)
//!  - staged parallel: parallel decode to materialized per-block
//!    columns, barrier, then parallel fused aggregation
//!  - morsel-fused: each worker decodes a block into fresh scratch,
//!    filters+aggregates it hot, emits (sum, count); tiny reduce
//!  - morsel-fused + reused scratch: per-thread scratch buffers, zero
//!    per-block allocation (the paper's steady-state shape)

use common::*;
use rayon::prelude::*;

const ROWS: usize = N_ORDERS;
const BLOCK: usize = 1 << 16;
const WANTED_STATUS: u8 = 2;

struct CompressedBlock {
    rows: usize,
    status: Vec<u8>,
    amount: Vec<u8>,
}

fn compress_blocks(orders: &Orders) -> Vec<CompressedBlock> {
    orders
        .status
        .chunks(BLOCK)
        .zip(orders.amount.chunks(BLOCK))
        .map(|(status, amount)| {
            let amount_bytes = amount
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<u8>>();
            CompressedBlock {
                rows: status.len(),
                status: lz4_flex::compress_prepend_size(status),
                amount: lz4_flex::compress_prepend_size(&amount_bytes),
            }
        })
        .collect()
}

fn decode_block(block: &CompressedBlock, status: &mut Vec<u8>, amount: &mut Vec<i64>) {
    status.clear();
    status.extend_from_slice(
        &lz4_flex::decompress_size_prepended(&block.status).expect("status block"),
    );
    let amount_bytes =
        lz4_flex::decompress_size_prepended(&block.amount).expect("amount block");
    amount.clear();
    amount.extend(
        amount_bytes
            .chunks_exact(8)
            .map(|bytes| i64::from_le_bytes(bytes.try_into().expect("8-byte lane"))),
    );
    assert_eq!(status.len(), block.rows);
}

/// e01's fused branchless filter+aggregate kernel.
#[inline]
fn fused_agg(status: &[u8], amount: &[i64]) -> (i64, u64) {
    let mut sum = 0i64;
    let mut count = 0u64;
    for (status, amount) in status.iter().zip(amount) {
        let hit = i64::from(*status == WANTED_STATUS);
        sum += hit * amount;
        count += hit as u64;
    }
    (sum, count)
}

fn checksum(sum: i64, count: u64) -> u64 {
    (sum as u64) ^ count.rotate_left(32)
}

fn staged_sequential(blocks: &[CompressedBlock]) -> u64 {
    let mut status = Vec::with_capacity(ROWS);
    let mut amount = Vec::with_capacity(ROWS);
    let mut block_status = Vec::with_capacity(BLOCK);
    let mut block_amount = Vec::with_capacity(BLOCK);
    for block in blocks {
        decode_block(block, &mut block_status, &mut block_amount);
        status.extend_from_slice(&block_status);
        amount.extend_from_slice(&block_amount);
    }
    let (sum, count) = fused_agg(&status, &amount);
    checksum(sum, count)
}

fn staged_parallel(blocks: &[CompressedBlock]) -> u64 {
    // Stage 1: parallel decode, everything materialized. Stage 2 barrier,
    // then parallel aggregation over the materialized blocks.
    let decoded: Vec<(Vec<u8>, Vec<i64>)> = blocks
        .par_iter()
        .map(|block| {
            let mut status = Vec::new();
            let mut amount = Vec::new();
            decode_block(block, &mut status, &mut amount);
            (status, amount)
        })
        .collect();
    let (sum, count) = decoded
        .par_iter()
        .map(|(status, amount)| fused_agg(status, amount))
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    checksum(sum, count)
}

fn morsel_fused(blocks: &[CompressedBlock]) -> u64 {
    let (sum, count) = blocks
        .par_iter()
        .map(|block| {
            let mut status = Vec::new();
            let mut amount = Vec::new();
            decode_block(block, &mut status, &mut amount);
            fused_agg(&status, &amount)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    checksum(sum, count)
}

fn morsel_fused_scratch(blocks: &[CompressedBlock]) -> u64 {
    let (sum, count) = blocks
        .par_iter()
        .map_init(
            || (Vec::with_capacity(BLOCK), Vec::with_capacity(BLOCK)),
            |(status, amount), block| {
                decode_block(block, status, amount);
                fused_agg(status, amount)
            },
        )
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    checksum(sum, count)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    println!("e17: {ROWS} rows, {BLOCK}-row LZ4 blocks, {threads} threads");
    let orders = gen_orders(ROWS, 0xe17);
    let blocks = compress_blocks(&orders);
    let compressed: usize = blocks
        .iter()
        .map(|block| block.status.len() + block.amount.len())
        .sum();
    println!(
        "compressed {} blocks, {:.1} MB (raw {:.1} MB)",
        blocks.len(),
        compressed as f64 / 1e6,
        (ROWS * 9) as f64 / 1e6
    );
    let results = [
        bench("staged sequential (decode all, then agg)", || {
            staged_sequential(&blocks)
        }),
        bench("staged parallel (decode || barrier || agg)", || {
            staged_parallel(&blocks)
        }),
        bench("morsel-fused (decode+agg per block)", || {
            morsel_fused(&blocks)
        }),
        bench("morsel-fused, per-thread scratch", || {
            morsel_fused_scratch(&blocks)
        }),
    ];
    check_consistency(&results);
}
