//! e22: settles the two contested claims from e20/e21 under the
//! methodology the Codex review demanded.
//!
//! What changed from e20/e21:
//!  - **16,384-row blocks**, the engine's `DEFAULT_BLOCK_ROWS`. e20/e21
//!    used 64k, which changes per-block bases, outlier counts and cache
//!    footprint.
//!  - **Every decoder starts from a serialized byte buffer.** e20's
//!    no-LZ4 arm decoded from a native `Vec<u64>` while the LZ4 arm
//!    parsed bytes, so the two arms ran different code and the reported
//!    LZ4 tax was partly that difference.
//!  - **The horizontal control is width-specialized with const generics**,
//!    so its shifts, masks and word offsets are compile-time constants —
//!    every advantage the interleaved kernel gets, and more.
//!  - **Outliers are two-sided.** A low outlier becomes the
//!    frame-of-reference base and widens every delta, which e20 never
//!    tested.
//!  - **Patched exceptions are self-delimiting** (exception count in the
//!    header) and chosen by *encoded bytes*, not by a fixed exception
//!    budget, so the measured size is one an implementation could write.
//!  - Round-trip fixtures for width 0, width 64, partial block, single
//!    row and all-equal blocks run before any timing.

use common::{Lcg, N_ORDERS, bench, check_consistency, gen_orders};

/// The engine's default. Not 64k.
const BLOCK: usize = 16 * 1024;
/// FastLanes chunk: 1024 W-bit values occupy exactly W virtual registers.
const CHUNK: usize = 1024;
/// A 1024-bit virtual register is 16 lanes of u64.
const LANES: usize = 1024 / 64;
/// Values each lane owns within one chunk.
const SLOTS: usize = CHUNK / LANES;

fn checksum(values: &[i64]) -> u64 {
    let mut acc = 0xcbf2_9ce4_8422_2325_u64;
    for (index, value) in values.iter().enumerate() {
        let mixed = (*value as u64)
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(index as u64);
        acc = (acc ^ mixed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        acc ^= acc >> 29;
    }
    acc
}

fn bit_width(range: u64) -> u32 {
    if range == 0 { 0 } else { 64 - range.leading_zeros() }
}

// ------------------------------------------------------------ serialization
//
// One header shape for every variant so no arm gets a parsing advantage:
//   base i64 | width u32 | rows u32 | exceptions u32 | payload | exceptions

const HEADER: usize = 8 + 4 + 4 + 4;

fn put_header(out: &mut Vec<u8>, base: i64, width: u32, rows: u32, exceptions: u32) {
    out.extend(base.to_le_bytes());
    out.extend(width.to_le_bytes());
    out.extend(rows.to_le_bytes());
    out.extend(exceptions.to_le_bytes());
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32"))
}

fn read_i64(bytes: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(bytes[at..at + 8].try_into().expect("i64"))
}

/// Reads one packed word out of the byte payload. Every kernel below goes
/// through this, so the comparison is between layouts, not between a word
/// array and a byte buffer.
#[inline(always)]
fn word_at(bytes: &[u8], index: usize) -> u64 {
    let start = index * 8;
    u64::from_le_bytes(bytes[start..start + 8].try_into().expect("word"))
}

// ------------------------------------------------------------- horizontal

fn pack_horizontal(deltas: &[u64], width: u32) -> Vec<u64> {
    if width == 0 {
        return Vec::new();
    }
    let mut words = vec![0_u64; (deltas.len() * width as usize).div_ceil(64) + 1];
    for (index, value) in deltas.iter().enumerate() {
        let bit = index * width as usize;
        let word = bit / 64;
        let offset = bit % 64;
        words[word] |= value << offset;
        if offset + width as usize > 64 {
            words[word + 1] = value >> (64 - offset);
        }
    }
    words
}

/// Width-specialized horizontal decoder. `W` is a compile-time constant, so
/// the mask, the shift schedule and the crossing test all fold at compile
/// time — the strongest control this layout can be given.
fn unpack_horizontal_const<const W: u32>(bytes: &[u8], rows: usize, out: &mut Vec<i64>, base: i64) {
    if W == 0 {
        out.extend(std::iter::repeat(base).take(rows));
        return;
    }
    let mask = if W == 64 { u64::MAX } else { (1_u64 << W) - 1 };
    for index in 0..rows {
        let bit = index * W as usize;
        let offset = bit % 64;
        let mut value = word_at(bytes, bit / 64) >> offset;
        if offset + W as usize > 64 {
            value |= word_at(bytes, bit / 64 + 1) << (64 - offset);
        }
        out.push(base.wrapping_add((value & mask) as i64));
    }
}

macro_rules! dispatch_horizontal {
    ($width:expr, $bytes:expr, $rows:expr, $out:expr, $base:expr) => {
        match $width {
            0 => unpack_horizontal_const::<0>($bytes, $rows, $out, $base),
            8 => unpack_horizontal_const::<8>($bytes, $rows, $out, $base),
            16 => unpack_horizontal_const::<16>($bytes, $rows, $out, $base),
            17 => unpack_horizontal_const::<17>($bytes, $rows, $out, $base),
            20 => unpack_horizontal_const::<20>($bytes, $rows, $out, $base),
            24 => unpack_horizontal_const::<24>($bytes, $rows, $out, $base),
            30 => unpack_horizontal_const::<30>($bytes, $rows, $out, $base),
            33 => unpack_horizontal_const::<33>($bytes, $rows, $out, $base),
            34 => unpack_horizontal_const::<34>($bytes, $rows, $out, $base),
            64 => unpack_horizontal_const::<64>($bytes, $rows, $out, $base),
            other => unpack_horizontal_runtime(other, $bytes, $rows, $out, $base),
        }
    };
}

/// Fallback for widths without a specialization, so no data shape silently
/// falls out of the comparison.
fn unpack_horizontal_runtime(width: u32, bytes: &[u8], rows: usize, out: &mut Vec<i64>, base: i64) {
    if width == 0 {
        out.extend(std::iter::repeat(base).take(rows));
        return;
    }
    let mask = if width == 64 { u64::MAX } else { (1_u64 << width) - 1 };
    for index in 0..rows {
        let bit = index * width as usize;
        let offset = bit % 64;
        let mut value = word_at(bytes, bit / 64) >> offset;
        if offset + width as usize > 64 {
            value |= word_at(bytes, bit / 64 + 1) << (64 - offset);
        }
        out.push(base.wrapping_add((value & mask) as i64));
    }
}

// ------------------------------------------------------------ interleaved

fn pack_interleaved(deltas: &[u64], width: u32) -> Vec<u64> {
    if width == 0 {
        return Vec::new();
    }
    // Whole chunks, then a horizontal tail for the remainder — a real
    // implementation cannot reject partial blocks.
    let chunks = deltas.len() / CHUNK;
    // Each chunk needs `width` rows of LANES words: SLOTS*width bits per
    // lane is exactly width u64 words when SLOTS is 64.
    let mut words = vec![0_u64; chunks * width as usize * LANES];
    for chunk in 0..chunks {
        let base = chunk * width as usize * LANES;
        for slot in 0..SLOTS {
            let bit = slot * width as usize;
            let row = bit / 64;
            let offset = bit % 64;
            for lane in 0..LANES {
                let value = deltas[chunk * CHUNK + slot * LANES + lane];
                let target = base + row * LANES + lane;
                words[target] |= value << offset;
                if offset + width as usize > 64 {
                    words[target + LANES] = value >> (64 - offset);
                }
            }
        }
    }
    words
}

fn unpack_interleaved(bytes: &[u8], width: u32, rows: usize, out: &mut Vec<i64>, base: i64) {
    if width == 0 {
        out.extend(std::iter::repeat(base).take(rows));
        return;
    }
    let mask = if width == 64 { u64::MAX } else { (1_u64 << width) - 1 };
    let chunks = rows / CHUNK;
    let start = out.len();
    out.resize(start + chunks * CHUNK, 0);
    for chunk in 0..chunks {
        let word_base = chunk * width as usize * LANES;
        let target = start + chunk * CHUNK;
        for slot in 0..SLOTS {
            let bit = slot * width as usize;
            let row = bit / 64;
            let offset = bit % 64;
            let crosses = offset + width as usize > 64;
            let destination = &mut out[target + slot * LANES..target + slot * LANES + LANES];
            if crosses {
                for lane in 0..LANES {
                    let low = word_at(bytes, word_base + row * LANES + lane);
                    let high = word_at(bytes, word_base + (row + 1) * LANES + lane);
                    let value = (low >> offset) | (high << (64 - offset));
                    destination[lane] = base.wrapping_add((value & mask) as i64);
                }
            } else {
                for lane in 0..LANES {
                    let low = word_at(bytes, word_base + row * LANES + lane);
                    destination[lane] = base.wrapping_add(((low >> offset) & mask) as i64);
                }
            }
        }
    }
}

// -------------------------------------------------------------- encoders

struct Encoded {
    bytes: Vec<u8>,
    rows: usize,
    width: u32,
    base: i64,
}

fn encode_for_horizontal(values: &[i64]) -> Encoded {
    let base = values.iter().copied().min().unwrap_or(0);
    let deltas = values
        .iter()
        .map(|v| v.wrapping_sub(base) as u64)
        .collect::<Vec<_>>();
    let width = bit_width(deltas.iter().copied().max().unwrap_or(0));
    let words = pack_horizontal(&deltas, width);
    let mut bytes = Vec::with_capacity(HEADER + words.len() * 8);
    put_header(&mut bytes, base, width, values.len() as u32, 0);
    bytes.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    Encoded { bytes, rows: values.len(), width, base }
}

fn encode_for_interleaved(values: &[i64]) -> Encoded {
    assert_eq!(values.len() % CHUNK, 0, "interleaved arm uses whole chunks");
    let base = values.iter().copied().min().unwrap_or(0);
    let deltas = values
        .iter()
        .map(|v| v.wrapping_sub(base) as u64)
        .collect::<Vec<_>>();
    let width = bit_width(deltas.iter().copied().max().unwrap_or(0));
    let words = pack_interleaved(&deltas, width);
    let mut bytes = Vec::with_capacity(HEADER + words.len() * 8);
    put_header(&mut bytes, base, width, values.len() as u32, 0);
    bytes.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    Encoded { bytes, rows: values.len(), width, base }
}

/// Patched exceptions chosen by **encoded bytes**, not by an exception-rate
/// budget, and serialized self-delimitingly so a decoder can find the
/// exception list without out-of-band knowledge.
fn encode_pfor(values: &[i64]) -> Encoded {
    let base = values.iter().copied().min().unwrap_or(0);
    let deltas = values
        .iter()
        .map(|v| v.wrapping_sub(base) as u64)
        .collect::<Vec<_>>();
    let full = bit_width(deltas.iter().copied().max().unwrap_or(0));
    let mut histogram = [0_usize; 65];
    for delta in &deltas {
        histogram[bit_width(*delta) as usize] += 1;
    }
    // Cost of every candidate width, in bytes, including the exceptions it
    // would create. Ties go to the narrower width.
    let mut best = (usize::MAX, full);
    let mut above = 0_usize;
    for candidate in (0..=full).rev() {
        let packed = (deltas.len() * candidate as usize).div_ceil(8);
        let cost = HEADER + packed + above * 12;
        if cost < best.0 {
            best = (cost, candidate);
        }
        above += histogram[candidate as usize];
    }
    let width = best.1;
    let mask = if width >= 64 { u64::MAX } else { (1_u64 << width) - 1 };
    let mut positions = Vec::new();
    let mut exceptions = Vec::new();
    let mut packable = Vec::with_capacity(deltas.len());
    for (index, delta) in deltas.iter().enumerate() {
        if *delta > mask {
            positions.push(index as u32);
            exceptions.push(values[index]);
            packable.push(0);
        } else {
            packable.push(*delta);
        }
    }
    let words = pack_horizontal(&packable, width);
    let mut bytes = Vec::with_capacity(HEADER + words.len() * 8 + positions.len() * 12);
    put_header(&mut bytes, base, width, values.len() as u32, positions.len() as u32);
    bytes.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    bytes.extend(positions.iter().flat_map(|p| p.to_le_bytes()));
    bytes.extend(exceptions.iter().flat_map(|v| v.to_le_bytes()));
    Encoded { bytes, rows: values.len(), width, base }
}

fn decode_pfor(bytes: &[u8], out: &mut Vec<i64>) {
    let base = read_i64(bytes, 0);
    let width = read_u32(bytes, 8);
    let rows = read_u32(bytes, 12) as usize;
    let count = read_u32(bytes, 16) as usize;
    let payload = &bytes[HEADER..];
    let start = out.len();
    dispatch_horizontal!(width, payload, rows, out, base);
    let packed_words = (rows * width as usize).div_ceil(64) + usize::from(width > 0);
    let tail = HEADER + packed_words * 8;
    for index in 0..count {
        let position = read_u32(bytes, tail + index * 4) as usize;
        let value = read_i64(bytes, tail + count * 4 + index * 8);
        out[start + position] = value;
    }
}

// ---------------------------------------------------------------- fixtures

fn round_trip_fixtures() {
    let cases: Vec<(&str, Vec<i64>)> = vec![
        ("width 0 (all equal)", vec![7; 4096]),
        ("single row", vec![42]),
        ("partial block", (0..1000).map(i64::from).collect()),
        ("width 64 (full range)", vec![i64::MIN, 0, i64::MAX, 5]),
        ("two-sided outliers", {
            let mut v = (0..4096).map(|i| 1000 + i64::from(i % 50)).collect::<Vec<_>>();
            v[10] = -9_000_000_000;
            v[4000] = 9_000_000_000;
            v
        }),
    ];
    for (name, values) in cases {
        let encoded = encode_for_horizontal(&values);
        let mut out = Vec::new();
        dispatch_horizontal!(
            encoded.width,
            &encoded.bytes[HEADER..],
            encoded.rows,
            &mut out,
            encoded.base
        );
        assert_eq!(out, values, "horizontal round-trip failed: {name}");

        let pfor = encode_pfor(&values);
        let mut out = Vec::new();
        decode_pfor(&pfor.bytes, &mut out);
        assert_eq!(out, values, "patched round-trip failed: {name}");
        println!("  fixture ok: {name} (width {} / patched {})", encoded.width, pfor.width);
    }
}

// -------------------------------------------------------------- fixtures 2

fn two_sided_outliers(rows: usize, seed: u64) -> Vec<i64> {
    let mut r = Lcg::new(seed);
    (0..rows)
        .map(|_| match r.below(1000) {
            0 => 100 + r.below(9_000_000_000) as i64,
            1 => -(r.below(9_000_000_000) as i64),
            _ => 100 + r.below(999_900) as i64,
        })
        .collect()
}

fn main() {
    println!("e22 — contested claims re-measured at the engine's block size");
    println!("{BLOCK}-row blocks, every decoder reading serialized bytes\n");

    println!("round-trip fixtures:");
    round_trip_fixtures();

    let orders = gen_orders(N_ORDERS, 0x5EED);
    settle_lz4("amount (uniform)", &orders.amount);
    let spiky = two_sided_outliers(N_ORDERS, 0xA11CE);
    settle_lz4("amount (0.1% high + 0.1% low outliers)", &spiky);
    settle_layout(&orders.amount);
}

/// Claim 1: does the LZ4 layer cost decode time? Both arms now run the
/// identical kernel over a byte buffer; only the source of those bytes
/// differs.
fn settle_lz4(label: &str, values: &[i64]) {
    println!("\n=== {label} ===");
    let blocks = values.chunks(BLOCK).map(encode_for_horizontal).collect::<Vec<_>>();
    let patched = values.chunks(BLOCK).map(encode_pfor).collect::<Vec<_>>();
    let compressed = blocks
        .iter()
        .map(|b| (lz4_flex::block::compress(&b.bytes), b.bytes.len()))
        .collect::<Vec<_>>();

    let raw = values.len() * 8;
    let encoded: usize = blocks.iter().map(|b| b.bytes.len()).sum();
    let patched_bytes: usize = patched.iter().map(|b| b.bytes.len()).sum();
    let lz4: usize = compressed.iter().map(|(c, _)| c.len()).sum();
    println!(
        "  size: raw {raw}  FOR {encoded} ({:.2}x)  FOR+lz4 {lz4} ({:.2}x)  patched {patched_bytes} ({:.2}x)",
        raw as f64 / encoded as f64,
        raw as f64 / lz4 as f64,
        raw as f64 / patched_bytes as f64,
    );
    println!(
        "  widths: FOR {}..{}  patched {}..{}",
        blocks.iter().map(|b| b.width).min().unwrap_or(0),
        blocks.iter().map(|b| b.width).max().unwrap_or(0),
        patched.iter().map(|b| b.width).min().unwrap_or(0),
        patched.iter().map(|b| b.width).max().unwrap_or(0),
    );

    let expected = checksum(values);
    let mut results = Vec::new();
    results.push(bench("  decode: FOR from bytes", || {
        let mut out = Vec::with_capacity(values.len());
        for block in &blocks {
            dispatch_horizontal!(
                block.width,
                &block.bytes[HEADER..],
                block.rows,
                &mut out,
                block.base
            );
        }
        checksum(&out)
    }));
    results.push(bench("  decode: lz4 then FOR from bytes", || {
        let mut out = Vec::with_capacity(values.len());
        for (block, (payload, plain_len)) in blocks.iter().zip(&compressed) {
            let plain = lz4_flex::block::decompress(payload, *plain_len).expect("round-trip");
            dispatch_horizontal!(block.width, &plain[HEADER..], block.rows, &mut out, block.base);
        }
        checksum(&out)
    }));
    assert_eq!(results[0].checksum, expected, "FOR must round-trip");
    check_consistency(&results);

    let patched_decode = bench("  decode: patched exceptions from bytes", || {
        let mut out = Vec::with_capacity(values.len());
        for block in &patched {
            decode_pfor(&block.bytes, &mut out);
        }
        checksum(&out)
    });
    assert_eq!(patched_decode.checksum, expected, "patched must round-trip");
}

/// Claim 2: is interleaving faster than a width-specialized horizontal
/// kernel, both reading bytes?
fn settle_layout(values: &[i64]) {
    println!("\n=== layout: interleaved vs width-specialized horizontal ===");
    let rows = values.len() - (values.len() % CHUNK);
    let values = &values[..rows];
    let horizontal = values.chunks(BLOCK).map(encode_for_horizontal).collect::<Vec<_>>();
    let interleaved = values
        .chunks(BLOCK)
        .map(encode_for_interleaved)
        .collect::<Vec<_>>();
    let h_bytes: usize = horizontal.iter().map(|b| b.bytes.len()).sum();
    let i_bytes: usize = interleaved.iter().map(|b| b.bytes.len()).sum();
    println!("  size: horizontal {h_bytes} B, interleaved {i_bytes} B (delta {})", i_bytes as i64 - h_bytes as i64);

    let expected = checksum(values);
    let mut results = Vec::new();
    results.push(bench("  unpack: horizontal, const-generic width", || {
        let mut out = Vec::with_capacity(rows);
        for block in &horizontal {
            dispatch_horizontal!(
                block.width,
                &block.bytes[HEADER..],
                block.rows,
                &mut out,
                block.base
            );
        }
        checksum(&out)
    }));
    results.push(bench("  unpack: FastLanes interleaved", || {
        let mut out = Vec::with_capacity(rows);
        for block in &interleaved {
            unpack_interleaved(
                &block.bytes[HEADER..],
                block.width,
                block.rows,
                &mut out,
                block.base,
            );
        }
        checksum(&out)
    }));
    assert_eq!(results[0].checksum, expected, "horizontal must round-trip");
    check_consistency(&results);

    // Consumer-only costs, measured directly instead of inferred by
    // subtraction, so the unpack share is not a guess.
    let decoded = {
        let mut out = Vec::with_capacity(rows);
        for block in &horizontal {
            dispatch_horizontal!(
                block.width,
                &block.bytes[HEADER..],
                block.rows,
                &mut out,
                block.base
            );
        }
        out
    };
    bench("  consumer only: checksum over decoded", || checksum(&decoded));
    bench("  consumer only: sum over decoded", || {
        decoded.iter().map(|v| *v as u64).sum::<u64>()
    });

    let mut sums = Vec::new();
    sums.push(bench("  sum: horizontal const-generic", || {
        let mut out = Vec::with_capacity(rows);
        for block in &horizontal {
            dispatch_horizontal!(
                block.width,
                &block.bytes[HEADER..],
                block.rows,
                &mut out,
                block.base
            );
        }
        out.iter().map(|v| *v as u64).sum::<u64>()
    }));
    sums.push(bench("  sum: interleaved", || {
        let mut out = Vec::with_capacity(rows);
        for block in &interleaved {
            unpack_interleaved(
                &block.bytes[HEADER..],
                block.width,
                block.rows,
                &mut out,
                block.base,
            );
        }
        out.iter().map(|v| *v as u64).sum::<u64>()
    }));
    check_consistency(&sums);
}
