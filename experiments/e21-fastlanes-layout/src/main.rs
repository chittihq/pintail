//! e21: does the FastLanes interleaved bit-packing layout actually decode
//! faster in Rust?
//!
//! Paper: Afroozeh & Boncz, "The FastLanes Compression Layout: Decoding
//! >100 Billion Integers per Second with Scalar Code" (PVLDB 16(9) 2023).
//! Their claim for a plain scalar path with no intrinsics is that treating
//! a 64-bit register as 64/T lanes gives exactly 64/T times the throughput
//! of conventional horizontal packing — 2x at T=32 — and that LLVM then
//! auto-vectorizes the same code further. Every published throughput
//! number is from C++/clang; the Rust port ships no measurements.
//!
//! PTSEG packs horizontally: value i occupies bits [i*W, (i+1)*W). This
//! measures that against the interleaved layout on our own data, at the
//! width our own amount column actually uses.
//!
//! Only the bit-level interleave is tested (FastLanes' mechanism 1a).
//! It preserves logical order, so it needs no selection vector and no
//! change to predicates or partial reads. The transposed layout that
//! reorders tuples — required only for DELTA and RLE — is not tested.

use common::{N_ORDERS, bench, check_consistency, gen_orders};

/// FastLanes' virtual register width. Not tunable: 1024 W-bit values
/// occupy exactly W registers, which is the identity the kernels rely on.
const CHUNK: usize = 1024;
/// Unpacked lane width. 1024/T lanes, so 32 lanes at T=32.
const T: usize = 32;
const LANES: usize = CHUNK / T;

fn checksum(values: &[u32]) -> u64 {
    let mut acc = 0xcbf2_9ce4_8422_2325_u64;
    for (index, value) in values.iter().enumerate() {
        let mixed = u64::from(*value)
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(index as u64);
        acc = (acc ^ mixed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        acc ^= acc >> 29;
    }
    acc
}

// ------------------------------------------------------- horizontal (ours)

/// Value i at bit offset i*W in one contiguous stream — what PTSEG writes.
fn pack_horizontal(values: &[u32], width: u32) -> Vec<u32> {
    let mut words = vec![0_u32; (values.len() * width as usize).div_ceil(32) + 1];
    for (index, value) in values.iter().enumerate() {
        let bit = index * width as usize;
        let word = bit / 32;
        let offset = bit % 32;
        words[word] |= value << offset;
        if offset + width as usize > 32 {
            words[word + 1] = value >> (32 - offset);
        }
    }
    words
}

fn unpack_horizontal(words: &[u32], width: u32, count: usize, out: &mut Vec<u32>) {
    let mask = if width == 32 { u32::MAX } else { (1 << width) - 1 };
    for index in 0..count {
        let bit = index * width as usize;
        let word = bit / 32;
        let offset = bit % 32;
        let mut value = words[word] >> offset;
        if offset + width as usize > 32 {
            value |= words[word + 1] << (32 - offset);
        }
        out.push(value & mask);
    }
}

// ------------------------------------------------------ interleaved (FL 1a)

/// Round-robin across lanes: value v lives in lane `v % LANES` at slot
/// `v / LANES`. Each lane owns an independent W*T-bit stream, and every
/// lane's extraction for a given slot uses the identical shift and mask —
/// which is what lets one straight-line loop cover all lanes.
fn pack_interleaved(values: &[u32], width: u32) -> Vec<u32> {
    assert_eq!(values.len() % CHUNK, 0, "whole chunks only");
    let chunks = values.len() / CHUNK;
    let mut words = vec![0_u32; chunks * width as usize * LANES];
    for chunk in 0..chunks {
        let base = chunk * width as usize * LANES;
        for slot in 0..T {
            let bit = slot * width as usize;
            let row = bit / 32;
            let offset = bit % 32;
            for lane in 0..LANES {
                let value = values[chunk * CHUNK + slot * LANES + lane];
                words[base + row * LANES + lane] |= value << offset;
                if offset + width as usize > 32 {
                    words[base + (row + 1) * LANES + lane] = value >> (32 - offset);
                }
            }
        }
    }
    words
}

/// The kernel under test. The inner loop walks `LANES` contiguous words
/// with a loop-invariant shift and mask and no cross-lane movement, which
/// is the shape the paper claims LLVM turns into vector code.
fn unpack_interleaved(words: &[u32], width: u32, count: usize, out: &mut Vec<u32>) {
    let mask = if width == 32 { u32::MAX } else { (1 << width) - 1 };
    let chunks = count / CHUNK;
    out.resize(count, 0);
    for chunk in 0..chunks {
        let base = chunk * width as usize * LANES;
        let target = chunk * CHUNK;
        for slot in 0..T {
            let bit = slot * width as usize;
            let row = bit / 32;
            let offset = bit % 32;
            let crosses = offset + width as usize > 32;
            let (low, high) = words[base + row * LANES..].split_at(LANES);
            let destination = &mut out[target + slot * LANES..target + slot * LANES + LANES];
            if crosses {
                for lane in 0..LANES {
                    destination[lane] =
                        ((low[lane] >> offset) | (high[lane] << (32 - offset))) & mask;
                }
            } else {
                for lane in 0..LANES {
                    destination[lane] = (low[lane] >> offset) & mask;
                }
            }
        }
    }
}

fn main() {
    println!("e21 — FastLanes interleaved bit-packing vs PTSEG's horizontal packing");
    println!("{N_ORDERS} values, T={T}, {LANES} lanes, {CHUNK}-value chunks\n");

    let orders = gen_orders(N_ORDERS, 0x5EED);
    // Frame of reference first, exactly as PTSEG does, so both layouts pack
    // the same deltas at the same width.
    let base = orders.amount.iter().copied().min().expect("rows");
    let deltas = orders
        .amount
        .iter()
        .map(|v| u32::try_from(v - base).expect("amount range fits u32"))
        .collect::<Vec<_>>();
    let rows = deltas.len() - (deltas.len() % CHUNK);
    let deltas = &deltas[..rows];
    let width = 32 - deltas.iter().copied().max().unwrap_or(0).leading_zeros();
    println!("frame of reference: base {base}, width {width} bits, {rows} values\n");

    let horizontal = pack_horizontal(deltas, width);
    let interleaved = pack_interleaved(deltas, width);
    println!(
        "packed size: horizontal {} B, interleaved {} B ({:+.2}%)\n",
        horizontal.len() * 4,
        interleaved.len() * 4,
        (interleaved.len() as f64 / horizontal.len() as f64 - 1.0) * 100.0,
    );

    let expected = checksum(deltas);
    let mut results = Vec::new();
    results.push(bench("unpack: horizontal (PTSEG today)", || {
        let mut out = Vec::with_capacity(rows);
        unpack_horizontal(&horizontal, width, rows, &mut out);
        checksum(&out)
    }));
    results.push(bench("unpack: FastLanes interleaved", || {
        let mut out = Vec::with_capacity(rows);
        unpack_interleaved(&interleaved, width, rows, &mut out);
        checksum(&out)
    }));
    assert_eq!(
        results[0].checksum, expected,
        "horizontal unpack must round-trip"
    );
    assert_eq!(
        results[1].checksum, expected,
        "interleaved unpack must round-trip in logical order"
    );
    check_consistency(&results);

    // Same question fused with the aggregate, since a decode-bound engine
    // never wants the decoded array for its own sake.
    let sum_horizontal = bench("sum: horizontal, unpack then add", || {
        let mut out = Vec::with_capacity(rows);
        unpack_horizontal(&horizontal, width, rows, &mut out);
        out.iter().map(|v| u64::from(*v)).sum::<u64>()
    });
    let sum_interleaved = bench("sum: interleaved, unpack then add", || {
        let mut out = Vec::with_capacity(rows);
        unpack_interleaved(&interleaved, width, rows, &mut out);
        out.iter().map(|v| u64::from(*v)).sum::<u64>()
    });
    check_consistency(&[sum_horizontal, sum_interleaved]);

    println!(
        "\nwidths sweep (unpack only, median ms): the paper's speedup is width-dependent"
    );
    for candidate in [4_u32, 8, 12, 16, 20, 24, 28] {
        let mask = if candidate == 32 {
            u32::MAX
        } else {
            (1 << candidate) - 1
        };
        let narrowed = deltas.iter().map(|v| v & mask).collect::<Vec<_>>();
        let h = pack_horizontal(&narrowed, candidate);
        let i = pack_interleaved(&narrowed, candidate);
        let mut out = Vec::with_capacity(rows);
        let hr = bench(&format!("  W={candidate:>2} horizontal"), || {
            out.clear();
            unpack_horizontal(&h, candidate, rows, &mut out);
            checksum(&out)
        });
        let ir = bench(&format!("  W={candidate:>2} interleaved"), || {
            let mut out = Vec::with_capacity(rows);
            unpack_interleaved(&i, candidate, rows, &mut out);
            checksum(&out)
        });
        check_consistency(&[hr, ir]);
    }
}
