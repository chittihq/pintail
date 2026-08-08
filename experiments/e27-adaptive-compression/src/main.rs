//! PROTOTYPE e27: should PTSEG retain LZ4 per encoded block only when it pays?
//!
//! The question is deliberately narrower than a production format change:
//! compare today's always-LZ4 policy, a global no-LZ4 policy, and two adaptive
//! policies over byte layouts matching PTSEG's actual 16,384-row payloads.
//! Every decode arm must reproduce the same position-sensitive checksum.

use common::{bench, check_consistency, Lcg};

const BLOCK_ROWS: usize = 16 * 1024;
const BLOCKS: usize = 256;
const ROWS: usize = BLOCK_ROWS * BLOCKS;

#[derive(Clone, Copy)]
enum AdaptivePolicy {
    AnySaving,
    FivePercent,
}

#[derive(Clone, Copy)]
enum DecodePolicy {
    Never,
    Always,
    Adaptive,
}

impl AdaptivePolicy {
    fn keeps_lz4(self, raw: usize, compressed: usize) -> bool {
        match self {
            Self::AnySaving => compressed < raw,
            Self::FivePercent => compressed.saturating_mul(100) <= raw.saturating_mul(95),
        }
    }
}

struct PreparedBlock {
    raw: Vec<u8>,
    lz4: Vec<u8>,
}

struct Shape {
    name: &'static str,
    blocks: Vec<PreparedBlock>,
}

fn main() {
    println!("e27 — adaptive LZ4 over exact PTSEG payload shapes");
    println!("{ROWS} rows, {BLOCK_ROWS}-row blocks, {BLOCKS} blocks per shape");
    println!("sizes exclude framing shared by every policy\n");

    run_shape(Shape::new("FOR bit-packed amount", amount_blocks()));
    run_shape(Shape::new(
        "delta-bit-packed primary key",
        delta_id_blocks(),
    ));
    run_shape(Shape::new(
        "mixed integer blocks, FOR + delta",
        mixed_integer_blocks(),
    ));
    run_shape(Shape::new("dictionary status, cyclic", status_blocks()));
    run_shape(Shape::new("dictionary region, random", region_blocks()));
    run_shape(Shape::new("plain random Float64", float_blocks()));
    run_shape(Shape::new("plain high-cardinality UTF-8", text_blocks()));
}

impl Shape {
    fn new(name: &'static str, raw: Vec<Vec<u8>>) -> Self {
        let blocks = raw
            .into_iter()
            .map(|raw| {
                let lz4 = lz4_flex::block::compress(&raw);
                let decoded = lz4_flex::block::decompress(&lz4, raw.len()).expect("LZ4 round-trip");
                assert_eq!(decoded, raw, "prepared block must round-trip");
                PreparedBlock { raw, lz4 }
            })
            .collect();
        Self { name, blocks }
    }
}

fn run_shape(shape: Shape) {
    let raw_bytes = shape
        .blocks
        .iter()
        .map(|block| block.raw.len())
        .sum::<usize>();
    let lz4_bytes = shape
        .blocks
        .iter()
        .map(|block| block.lz4.len())
        .sum::<usize>();
    let any = policy_size(&shape.blocks, AdaptivePolicy::AnySaving);
    let five = policy_size(&shape.blocks, AdaptivePolicy::FivePercent);

    assert!(any.bytes <= raw_bytes && any.bytes <= lz4_bytes);
    println!("=== {} ===", shape.name);
    println!(
        "  never {:>10} B | always {:>10} B ({:>6.2}% of raw)",
        raw_bytes,
        lz4_bytes,
        percent(lz4_bytes, raw_bytes),
    );
    println!(
        "  adaptive any-saving {:>10} B, LZ4 {:>3}/{BLOCKS} blocks | 5% threshold {:>10} B, LZ4 {:>3}/{BLOCKS}",
        any.bytes, any.compressed_blocks, five.bytes, five.compressed_blocks,
    );

    let compressed = bench("  encode: always LZ4", || {
        shape
            .blocks
            .iter()
            .map(|block| lz4_flex::block::compress(&block.raw).len() as u64)
            .sum()
    });
    let selected = bench("  encode: LZ4 then adaptive select", || {
        shape
            .blocks
            .iter()
            .map(|block| {
                let lz4 = lz4_flex::block::compress(&block.raw);
                if AdaptivePolicy::AnySaving.keeps_lz4(block.raw.len(), lz4.len()) {
                    lz4.len() as u64
                } else {
                    block.raw.len() as u64
                }
            })
            .sum()
    });
    assert_eq!(compressed.checksum, lz4_bytes as u64);
    assert_eq!(selected.checksum, any.bytes as u64);

    let mut decoded = Vec::new();
    let never = bench("  decode: never LZ4", || {
        decode_checksum(&shape.blocks, DecodePolicy::Never, &mut decoded)
    });
    let always = bench("  decode: always LZ4", || {
        decode_checksum(&shape.blocks, DecodePolicy::Always, &mut decoded)
    });
    let adaptive = bench("  decode: adaptive any-saving", || {
        decode_checksum(&shape.blocks, DecodePolicy::Adaptive, &mut decoded)
    });
    check_consistency(&[never, always, adaptive]);
    println!();
}

struct PolicySize {
    bytes: usize,
    compressed_blocks: usize,
}

fn policy_size(blocks: &[PreparedBlock], policy: AdaptivePolicy) -> PolicySize {
    blocks.iter().fold(
        PolicySize {
            bytes: 0,
            compressed_blocks: 0,
        },
        |mut result, block| {
            if policy.keeps_lz4(block.raw.len(), block.lz4.len()) {
                result.bytes += block.lz4.len();
                result.compressed_blocks += 1;
            } else {
                result.bytes += block.raw.len();
            }
            result
        },
    )
}

fn decode_checksum(blocks: &[PreparedBlock], policy: DecodePolicy, scratch: &mut Vec<u8>) -> u64 {
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut position = 0_u64;
    for block in blocks {
        let use_lz4 = match policy {
            DecodePolicy::Never => false,
            DecodePolicy::Always => true,
            DecodePolicy::Adaptive => {
                AdaptivePolicy::AnySaving.keeps_lz4(block.raw.len(), block.lz4.len())
            }
        };
        let bytes = if use_lz4 {
            scratch.clear();
            scratch.resize(block.raw.len(), 0);
            let written = lz4_flex::block::decompress_into(&block.lz4, scratch.as_mut_slice())
                .expect("LZ4 decode");
            assert_eq!(written, block.raw.len());
            scratch.as_slice()
        } else {
            block.raw.as_slice()
        };
        for byte in bytes {
            checksum = (checksum ^ (u64::from(*byte) + position)).wrapping_mul(0x100_0000_01b3);
            position = position.wrapping_add(1);
        }
    }
    checksum
}

fn percent(part: usize, whole: usize) -> f64 {
    part as f64 * 100.0 / whole as f64
}

fn amount_blocks() -> Vec<Vec<u8>> {
    let mut random = Lcg::new(0xA11CE);
    (0..BLOCKS)
        .map(|_| {
            let values = (0..BLOCK_ROWS)
                .map(|_| 100_i64 + random.below(999_900) as i64)
                .collect::<Vec<_>>();
            encode_for_i64(&values)
        })
        .collect()
}

fn delta_id_blocks() -> Vec<Vec<u8>> {
    (0..BLOCKS)
        .map(|block| {
            let first = block as u64 * BLOCK_ROWS as u64 + 1;
            let values = (0..BLOCK_ROWS)
                .map(|offset| first + offset as u64)
                .collect::<Vec<_>>();
            encode_delta_u64(&values)
        })
        .collect()
}

fn mixed_integer_blocks() -> Vec<Vec<u8>> {
    let mut random = Lcg::new(0xD15EA5E);
    (0..BLOCKS)
        .map(|block| {
            if block % 2 == 0 {
                let values = (0..BLOCK_ROWS)
                    .map(|_| 100_i64 + random.below(999_900) as i64)
                    .collect::<Vec<_>>();
                encode_for_i64(&values)
            } else {
                let first = block as u64 * BLOCK_ROWS as u64 + 1;
                let values = (0..BLOCK_ROWS)
                    .map(|offset| first + offset as u64)
                    .collect::<Vec<_>>();
                encode_delta_u64(&values)
            }
        })
        .collect()
}

fn status_blocks() -> Vec<Vec<u8>> {
    const VALUES: [&[u8]; 5] = [b"pending", b"paid", b"shipped", b"refunded", b"cancelled"];
    (0..BLOCKS)
        .map(|block| {
            let start = block * BLOCK_ROWS;
            encode_dictionary(&VALUES, |row| ((start + row) % VALUES.len()) as u32)
        })
        .collect()
}

fn region_blocks() -> Vec<Vec<u8>> {
    const VALUES: [&[u8]; 8] = [
        b"north", b"south", b"east", b"west", b"central", b"coastal", b"metro", b"rural",
    ];
    let mut random = Lcg::new(0xBEEF);
    (0..BLOCKS)
        .map(|_| {
            let codes = (0..BLOCK_ROWS)
                .map(|_| random.below(VALUES.len() as u64) as u32)
                .collect::<Vec<_>>();
            encode_dictionary(&VALUES, |row| codes[row])
        })
        .collect()
}

fn float_blocks() -> Vec<Vec<u8>> {
    let mut random = Lcg::new(0xF10A7);
    (0..BLOCKS)
        .map(|_| {
            let mut out = Vec::with_capacity(BLOCK_ROWS * 8);
            for _ in 0..BLOCK_ROWS {
                push_u64(&mut out, random.next_u64());
            }
            out
        })
        .collect()
}

fn text_blocks() -> Vec<Vec<u8>> {
    let mut random = Lcg::new(0x7E57);
    (0..BLOCKS)
        .map(|block| {
            let mut out = Vec::with_capacity(BLOCK_ROWS * 32);
            for row in 0..BLOCK_ROWS {
                let id = block * BLOCK_ROWS + row;
                let value = format!("customer-{id:08x}-{:016x}", random.next_u64());
                push_bytes(&mut out, value.as_bytes());
            }
            out
        })
        .collect()
}

fn encode_for_i64(values: &[i64]) -> Vec<u8> {
    let base = values.iter().copied().min().unwrap_or(0);
    let normalized = values
        .iter()
        .map(|value| u64::try_from(*value - base).expect("amount range fits u64"))
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    push_i64(&mut out, base);
    encode_packed(&mut out, &normalized);
    out
}

fn encode_delta_u64(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, values[0]);
    let deltas = values
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    encode_packed(&mut out, &deltas);
    out
}

fn encode_dictionary<const N: usize>(values: &[&[u8]; N], code: impl Fn(usize) -> u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCK_ROWS * 4 + 128);
    push_u32(&mut out, N as u32);
    for value in values {
        push_bytes(&mut out, value);
    }
    for row in 0..BLOCK_ROWS {
        push_u32(&mut out, code(row));
    }
    out
}

fn encode_packed(out: &mut Vec<u8>, values: &[u64]) {
    let maximum = values.iter().copied().max().unwrap_or(0);
    let width = (u64::BITS - maximum.leading_zeros()) as u8;
    out.push(width);
    let total_bits = values.len() * usize::from(width);
    let mut packed = vec![0_u8; total_bits.div_ceil(8)];
    for (value_index, value) in values.iter().enumerate() {
        for bit in 0..width {
            if value & (1_u64 << bit) != 0 {
                let position = value_index * usize::from(width) + usize::from(bit);
                packed[position / 8] |= 1 << (position % 8);
            }
        }
    }
    push_bytes(out, &packed);
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn push_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
