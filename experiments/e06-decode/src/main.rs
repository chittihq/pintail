//! e06: Is scanning compressed data actually free?
//!
//! SUM over 20M i64 values (range [100, 1M) → 20 bits after frame-of-reference)
//! stored four ways: plain i64, FOR+bit-packed (FastLanes-style scalar unpack,
//! autovectorizable), lz4 over raw, lz4 over packed. Compression ratios printed.

use common::*;

const BLOCK: usize = 1024;
const BITS: u64 = 20;
const MASK: u64 = (1 << BITS) - 1;

struct Packed {
    words: Vec<u64>, // BLOCK * BITS / 64 words per block, contiguous
    mins: Vec<i64>,
    len: usize,
}

fn pack(values: &[i64]) -> Packed {
    let words_per_block = BLOCK * BITS as usize / 64;
    let blocks = values.len().div_ceil(BLOCK);
    let mut packed = Packed {
        words: vec![0u64; blocks * words_per_block],
        mins: Vec::with_capacity(blocks),
        len: values.len(),
    };
    for (b, chunk) in values.chunks(BLOCK).enumerate() {
        let min = *chunk.iter().min().unwrap();
        packed.mins.push(min);
        let base = b * words_per_block;
        for (i, &v) in chunk.iter().enumerate() {
            let delta = (v - min) as u64 & MASK;
            let bit = i * BITS as usize;
            let word = base + (bit >> 6);
            let offset = (bit & 63) as u64;
            packed.words[word] |= delta << offset;
            if offset + BITS > 64 {
                packed.words[word + 1] |= delta >> (64 - offset);
            }
        }
    }
    packed
}

#[inline]
fn sum_block_fused(words: &[u64], min: i64, count: usize) -> i64 {
    let mut sum = 0i64;
    for i in 0..count {
        let bit = i * BITS as usize;
        let word = bit >> 6;
        let offset = (bit & 63) as u64;
        let mut delta = words[word] >> offset;
        if offset + BITS > 64 {
            delta |= words[word + 1] << (64 - offset);
        }
        sum += (delta & MASK) as i64;
    }
    sum + min * count as i64
}

fn main() {
    println!("e06-decode  N = {N_ORDERS}");
    let o = gen_orders(N_ORDERS, 42);
    let amount = &o.amount;
    let raw_bytes = amount.len() * 8;

    let packed = pack(amount);
    let words_per_block = BLOCK * BITS as usize / 64;
    let packed_bytes = packed.words.len() * 8 + packed.mins.len() * 8;

    // lz4 over raw blocks and over packed blocks
    let mut lz4_raw: Vec<Vec<u8>> = Vec::new();
    for chunk in amount.chunks(BLOCK) {
        let mut bytes = Vec::with_capacity(chunk.len() * 8);
        for &v in chunk {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        lz4_raw.push(lz4_flex::compress_prepend_size(&bytes));
    }
    let lz4_raw_bytes: usize = lz4_raw.iter().map(|b| b.len()).sum();
    let mut lz4_packed: Vec<Vec<u8>> = Vec::new();
    for (b, chunk) in packed.words.chunks(words_per_block).enumerate() {
        let mut bytes = Vec::with_capacity(chunk.len() * 8 + 8);
        bytes.extend_from_slice(&packed.mins[b].to_le_bytes());
        for &w in chunk {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        lz4_packed.push(lz4_flex::compress_prepend_size(&bytes));
    }
    let lz4_packed_bytes: usize = lz4_packed.iter().map(|b| b.len()).sum();

    println!(
        "sizes: raw {:.1} MB | packed {:.1} MB ({:.2}x) | lz4(raw) {:.1} MB ({:.2}x) | lz4(packed) {:.1} MB ({:.2}x)",
        raw_bytes as f64 / 1e6,
        packed_bytes as f64 / 1e6,
        raw_bytes as f64 / packed_bytes as f64,
        lz4_raw_bytes as f64 / 1e6,
        raw_bytes as f64 / lz4_raw_bytes as f64,
        lz4_packed_bytes as f64 / 1e6,
        raw_bytes as f64 / lz4_packed_bytes as f64,
    );

    let mut rs = vec![];
    rs.push(bench("plain Vec<i64> scan sum", || {
        let mut s = 0i64;
        for &v in amount {
            s += v;
        }
        s as u64
    }));

    rs.push(bench("FOR+bitpack: fused unpack-sum", || {
        let mut s = 0i64;
        let mut remaining = packed.len;
        for (b, min) in packed.mins.iter().enumerate() {
            let count = remaining.min(BLOCK);
            remaining -= count;
            s += sum_block_fused(
                &packed.words[b * words_per_block..(b + 1) * words_per_block],
                *min,
                count,
            );
        }
        s as u64
    }));

    rs.push(bench("FOR+bitpack: unpack to buffer, then sum", || {
        let mut s = 0i64;
        let mut buffer = [0i64; BLOCK];
        let mut remaining = packed.len;
        for (b, min) in packed.mins.iter().enumerate() {
            let count = remaining.min(BLOCK);
            remaining -= count;
            let words = &packed.words[b * words_per_block..(b + 1) * words_per_block];
            for i in 0..count {
                let bit = i * BITS as usize;
                let word = bit >> 6;
                let offset = (bit & 63) as u64;
                let mut delta = words[word] >> offset;
                if offset + BITS > 64 {
                    delta |= words[word + 1] << (64 - offset);
                }
                buffer[i] = (delta & MASK) as i64 + min;
            }
            for &v in &buffer[..count] {
                s += v;
            }
        }
        s as u64
    }));

    rs.push(bench("lz4(raw): decompress + sum", || {
        let mut s = 0i64;
        for block in &lz4_raw {
            let bytes = lz4_flex::decompress_size_prepended(block).unwrap();
            for chunk in bytes.chunks_exact(8) {
                s += i64::from_le_bytes(chunk.try_into().unwrap());
            }
        }
        s as u64
    }));

    rs.push(bench("lz4(packed): decompress + fused unpack-sum", || {
        let mut s = 0i64;
        let mut remaining = packed.len;
        for block in &lz4_packed {
            let bytes = lz4_flex::decompress_size_prepended(block).unwrap();
            let min = i64::from_le_bytes(bytes[..8].try_into().unwrap());
            let mut words = [0u64; 320];
            for (w, chunk) in words.iter_mut().zip(bytes[8..].chunks_exact(8)) {
                *w = u64::from_le_bytes(chunk.try_into().unwrap());
            }
            let count = remaining.min(BLOCK);
            remaining -= count;
            s += sum_block_fused(&words[..], min, count);
        }
        s as u64
    }));

    check_consistency(&rs);
}
