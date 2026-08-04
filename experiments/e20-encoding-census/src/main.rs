//! e20: what do PTSEG's encodings actually cost, and what would the
//! missing ones buy?
//!
//! PTSEG picks one encoding per block: RunLength only when every cell in
//! the block is identical, Dictionary for text under 10% distinct,
//! DeltaBitPacked for monotonic integers, FOR+bit-packed for the rest,
//! Plain otherwise — then LZ4 over the block (Zstd at the coldest tier).
//! Three candidate encodings are absent, and each is contested:
//!
//!  - run-end over dictionary codes, instead of one code per row;
//!  - patched exceptions, so one outlier does not widen a whole block;
//!  - any float encoding at all — Float64 falls through to Plain.
//!
//! This measures size AND decode throughput for each, because our cold
//! path is decode-bound: a smaller block that decodes slower is a loss.
//! Every decoder must reconstruct the column exactly (checksum over the
//! decoded values, not over the encoded bytes).
//!
//! Data shape matters more than any single number here. The benchmark's
//! status column cycles every 5 rows, which is deliberately hostile to
//! run-length; real replicated tables sorted by primary key often have
//! clustered secondary columns instead. Both shapes are measured, and
//! the gap between them is the finding.

use common::{Lcg, N_ORDERS, N_REGIONS, N_STATUS, N_USERS, bench, check_consistency, gen_orders};

const BLOCK: usize = 1 << 16;

// ---------------------------------------------------------------- checksums

/// Position-mixing checksum. A plain XOR fold collides across permutations,
/// which is exactly the bug a reordering encoder would introduce.
fn checksum_i64(values: &[i64]) -> u64 {
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

fn checksum_f64(values: &[f64]) -> u64 {
    let bits = values.iter().map(|v| v.to_bits() as i64).collect::<Vec<_>>();
    checksum_i64(&bits)
}

// ------------------------------------------------------------ bit packing

fn bit_width(range: u64) -> u32 {
    if range == 0 { 0 } else { 64 - range.leading_zeros() }
}

fn pack(values: &[u64], width: u32) -> Vec<u64> {
    if width == 0 {
        return Vec::new();
    }
    let mut words = vec![0_u64; (values.len() * width as usize).div_ceil(64) + 1];
    for (index, value) in values.iter().enumerate() {
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

/// Unpacks straight out of a byte buffer, which is what a decoder fed by a
/// block decompressor actually has in hand. Reading the words back out of
/// the original in-memory array instead would measure a pipeline nobody
/// runs and would hide the decompressor's cache footprint.
fn unpack_from_bytes(bytes: &[u8], width: u32, count: usize, out: &mut Vec<u64>) {
    out.clear();
    if width == 0 {
        out.resize(count, 0);
        return;
    }
    let word = |index: usize| -> u64 {
        let start = index * 8;
        u64::from_le_bytes(bytes[start..start + 8].try_into().expect("whole word"))
    };
    let mask = if width == 64 { u64::MAX } else { (1 << width) - 1 };
    for index in 0..count {
        let bit = index * width as usize;
        let offset = bit % 64;
        let mut value = word(bit / 64) >> offset;
        if offset + width as usize > 64 {
            value |= word(bit / 64 + 1) << (64 - offset);
        }
        out.push(value & mask);
    }
}

fn unpack_into(words: &[u64], width: u32, count: usize, out: &mut Vec<u64>) {
    out.clear();
    if width == 0 {
        out.resize(count, 0);
        return;
    }
    let mask = if width == 64 { u64::MAX } else { (1 << width) - 1 };
    for index in 0..count {
        let bit = index * width as usize;
        let word = bit / 64;
        let offset = bit % 64;
        let mut value = words[word] >> offset;
        if offset + width as usize > 64 {
            value |= words[word + 1] << (64 - offset);
        }
        out.push(value & mask);
    }
}

// ------------------------------------------------------------- encodings

/// Frame of reference + bit packing: what PTSEG does today.
struct ForBlock {
    base: i64,
    width: u32,
    words: Vec<u64>,
    rows: usize,
}

fn encode_for(values: &[i64]) -> ForBlock {
    let base = values.iter().copied().min().unwrap_or(0);
    let deltas = values
        .iter()
        .map(|v| v.wrapping_sub(base) as u64)
        .collect::<Vec<_>>();
    let width = bit_width(deltas.iter().copied().max().unwrap_or(0));
    ForBlock {
        base,
        width,
        words: pack(&deltas, width),
        rows: values.len(),
    }
}

impl ForBlock {
    fn bytes(&self) -> usize {
        self.words.len() * 8 + 16
    }

    fn decode_into(&self, scratch: &mut Vec<u64>, out: &mut Vec<i64>) {
        unpack_into(&self.words, self.width, self.rows, scratch);
        out.extend(scratch.iter().map(|d| self.base.wrapping_add(*d as i64)));
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.base.to_le_bytes().to_vec();
        bytes.extend(self.width.to_le_bytes());
        bytes.extend(self.words.iter().flat_map(|w| w.to_le_bytes()));
        bytes
    }
}

/// Frame of reference with patched exceptions: the width is chosen to fit
/// the bulk of the block and the stragglers are stored out of line, so one
/// outlier cannot widen every value.
struct PforBlock {
    base: i64,
    width: u32,
    words: Vec<u64>,
    rows: usize,
    exception_positions: Vec<u32>,
    exception_values: Vec<i64>,
}

fn encode_pfor(values: &[i64], target_exception_rate: f64) -> PforBlock {
    let base = values.iter().copied().min().unwrap_or(0);
    let deltas = values
        .iter()
        .map(|v| v.wrapping_sub(base) as u64)
        .collect::<Vec<_>>();
    // Pick the narrowest width leaving at most the target share as
    // exceptions: the histogram of required widths, walked from the bottom.
    let mut histogram = [0_usize; 65];
    for delta in &deltas {
        histogram[bit_width(*delta) as usize] += 1;
    }
    let budget = (deltas.len() as f64 * target_exception_rate) as usize;
    let mut above = 0;
    let mut width = 64_u32;
    for candidate in (0..=64_usize).rev() {
        if above > budget {
            width = candidate as u32 + 1;
            break;
        }
        above += histogram[candidate];
        width = candidate as u32;
    }
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1 << width) - 1
    };
    let mut exception_positions = Vec::new();
    let mut exception_values = Vec::new();
    let mut packable = Vec::with_capacity(deltas.len());
    for (index, delta) in deltas.iter().enumerate() {
        if *delta > mask {
            exception_positions.push(index as u32);
            exception_values.push(values[index]);
            packable.push(0);
        } else {
            packable.push(*delta);
        }
    }
    PforBlock {
        base,
        width,
        words: pack(&packable, width),
        rows: values.len(),
        exception_positions,
        exception_values,
    }
}

impl PforBlock {
    fn bytes(&self) -> usize {
        self.words.len() * 8 + 16 + self.exception_positions.len() * 12
    }

    fn decode_into(&self, scratch: &mut Vec<u64>, out: &mut Vec<i64>) {
        unpack_into(&self.words, self.width, self.rows, scratch);
        let start = out.len();
        out.extend(scratch.iter().map(|d| self.base.wrapping_add(*d as i64)));
        // Patch pass: proportional to the exception count, not the block.
        for (position, value) in self
            .exception_positions
            .iter()
            .zip(&self.exception_values)
        {
            out[start + *position as usize] = *value;
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.base.to_le_bytes().to_vec();
        bytes.extend(self.width.to_le_bytes());
        bytes.extend(self.words.iter().flat_map(|w| w.to_le_bytes()));
        bytes.extend(self.exception_positions.iter().flat_map(|p| p.to_le_bytes()));
        bytes.extend(self.exception_values.iter().flat_map(|v| v.to_le_bytes()));
        bytes
    }
}

/// Dictionary codes, bit-packed — one code per row.
struct DictBlock {
    values: Vec<i64>,
    width: u32,
    words: Vec<u64>,
    rows: usize,
}

fn encode_dict(values: &[i64]) -> DictBlock {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let codes = values
        .iter()
        .map(|v| distinct.binary_search(v).expect("value is in dictionary") as u64)
        .collect::<Vec<_>>();
    let width = bit_width(distinct.len().saturating_sub(1) as u64);
    DictBlock {
        width,
        words: pack(&codes, width),
        rows: values.len(),
        values: distinct,
    }
}

impl DictBlock {
    fn bytes(&self) -> usize {
        self.words.len() * 8 + self.values.len() * 8 + 8
    }

    fn decode_into(&self, scratch: &mut Vec<u64>, out: &mut Vec<i64>) {
        unpack_into(&self.words, self.width, self.rows, scratch);
        out.extend(scratch.iter().map(|code| self.values[*code as usize]));
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        bytes.extend(self.words.iter().flat_map(|w| w.to_le_bytes()));
        bytes
    }
}

/// Run-end encoding over dictionary codes: (code, run_end) pairs. Arrow's
/// REE layout stores ends rather than lengths so a random access is one
/// binary search instead of a prefix sum.
struct RunEndBlock {
    values: Vec<i64>,
    codes: Vec<u32>,
    ends: Vec<u32>,
    rows: usize,
}

fn encode_run_end(values: &[i64]) -> RunEndBlock {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let mut codes = Vec::new();
    let mut ends = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let code = distinct.binary_search(value).expect("in dictionary") as u32;
        if codes.last() == Some(&code) {
            *ends.last_mut().expect("run in progress") = index as u32 + 1;
        } else {
            codes.push(code);
            ends.push(index as u32 + 1);
        }
    }
    RunEndBlock {
        values: distinct,
        codes,
        ends,
        rows: values.len(),
    }
}

impl RunEndBlock {
    fn bytes(&self) -> usize {
        self.codes.len() * 4 + self.ends.len() * 4 + self.values.len() * 8
    }

    fn runs(&self) -> usize {
        self.codes.len()
    }

    fn decode_into(&self, out: &mut Vec<i64>) {
        let mut start = 0_u32;
        for (code, end) in self.codes.iter().zip(&self.ends) {
            let value = self.values[*code as usize];
            for _ in start..*end {
                out.push(value);
            }
            start = *end;
        }
        debug_assert_eq!(out.len() % self.rows, 0);
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        bytes.extend(self.codes.iter().flat_map(|c| c.to_le_bytes()));
        bytes.extend(self.ends.iter().flat_map(|e| e.to_le_bytes()));
        bytes
    }

    /// The reason run-end is interesting beyond size: a predicate is one
    /// test per run, and the matching row count is arithmetic.
    fn count_matching(&self, wanted: i64) -> u64 {
        let mut start = 0_u32;
        let mut count = 0_u64;
        for (code, end) in self.codes.iter().zip(&self.ends) {
            if self.values[*code as usize] == wanted {
                count += u64::from(*end - start);
            }
            start = *end;
        }
        count
    }
}

/// Pseudodecimal: doubles that are really decimals become an integer
/// mantissa plus a shared exponent, then FOR+bit-pack. This is the core
/// idea shared by BtrBlocks' pseudodecimal and ALP's first scheme.
struct PseudoDecimalBlock {
    exponent: u32,
    integers: ForBlock,
    exception_positions: Vec<u32>,
    exception_values: Vec<f64>,
}

fn encode_pseudodecimal(values: &[f64]) -> Option<PseudoDecimalBlock> {
    // Smallest exponent that makes (nearly) every value integral.
    for exponent in 0..=8_u32 {
        let scale = 10_f64.powi(exponent as i32);
        let mut integers = Vec::with_capacity(values.len());
        let mut exception_positions = Vec::new();
        let mut exception_values = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let scaled = value * scale;
            let rounded = scaled.round();
            if (rounded / scale - value).abs() < f64::EPSILON && rounded.abs() < 9e18 {
                integers.push(rounded as i64);
            } else {
                exception_positions.push(index as u32);
                exception_values.push(*value);
                integers.push(0);
            }
        }
        if exception_positions.len() * 20 < values.len() {
            return Some(PseudoDecimalBlock {
                exponent,
                integers: encode_for(&integers),
                exception_positions,
                exception_values,
            });
        }
    }
    None
}

impl PseudoDecimalBlock {
    fn bytes(&self) -> usize {
        self.integers.bytes() + 4 + self.exception_positions.len() * 12
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.exponent.to_le_bytes().to_vec();
        bytes.extend(self.integers.to_bytes());
        bytes.extend(self.exception_positions.iter().flat_map(|p| p.to_le_bytes()));
        bytes.extend(self.exception_values.iter().flat_map(|v| v.to_le_bytes()));
        bytes
    }

    fn decode_into(&self, scratch: &mut Vec<u64>, ints: &mut Vec<i64>, out: &mut Vec<f64>) {
        ints.clear();
        self.integers.decode_into(scratch, ints);
        let scale = 10_f64.powi(self.exponent as i32);
        let start = out.len();
        out.extend(ints.iter().map(|v| *v as f64 / scale));
        for (position, value) in self
            .exception_positions
            .iter()
            .zip(&self.exception_values)
        {
            out[start + *position as usize] = *value;
        }
    }
}

// ------------------------------------------------------------------ data

fn lz4_size(bytes: &[u8]) -> usize {
    lz4_flex::block::compress(bytes).len()
}

fn as_bytes_i64(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn as_bytes_f64(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A clustered variant of a low-cardinality column: the same values and the
/// same cardinality, but arranged in runs the way a secondary column looks
/// once a table is sorted by its primary key.
fn clustered(values: &[i64], seed: u64) -> Vec<i64> {
    let mut r = Lcg::new(seed);
    let distinct = {
        let mut d = values.to_vec();
        d.sort_unstable();
        d.dedup();
        d
    };
    let mut out = Vec::with_capacity(values.len());
    while out.len() < values.len() {
        let value = distinct[(r.below(distinct.len() as u64)) as usize];
        let run = 20 + r.below(180);
        for _ in 0..run {
            if out.len() == values.len() {
                break;
            }
            out.push(value);
        }
    }
    out
}

/// Uniform amounts with a thin tail of large values — the shape that makes
/// a single block-wide bit width expensive.
fn amounts_with_outliers(rows: usize, seed: u64) -> Vec<i64> {
    let mut r = Lcg::new(seed);
    (0..rows)
        .map(|_| {
            if r.below(1000) == 0 {
                100 + r.below(9_000_000_000) as i64
            } else {
                100 + r.below(999_900) as i64
            }
        })
        .collect()
}

fn prices(rows: usize, seed: u64) -> Vec<f64> {
    let mut r = Lcg::new(seed);
    (0..rows)
        .map(|_| (100 + r.below(999_900)) as f64 / 100.0)
        .collect()
}

fn ratios(rows: usize, seed: u64) -> Vec<f64> {
    let mut r = Lcg::new(seed);
    (0..rows)
        .map(|_| r.next_u64() as f64 / u64::MAX as f64)
        .collect()
}

// --------------------------------------------------------------- reports

struct Sizing {
    name: &'static str,
    encoded: usize,
    with_lz4: usize,
}

fn report_sizes(column: &str, raw: usize, rows: usize, sizes: &[Sizing]) {
    println!("\n{column}: {rows} rows, {raw} B raw ({:.2} B/row)", raw as f64 / rows as f64);
    println!(
        "  {:<34} {:>12} {:>9} {:>12} {:>9}",
        "encoding", "encoded B", "vs raw", "+lz4 B", "vs raw"
    );
    for size in sizes {
        println!(
            "  {:<34} {:>12} {:>8.2}x {:>12} {:>8.2}x",
            size.name,
            size.encoded,
            raw as f64 / size.encoded as f64,
            size.with_lz4,
            raw as f64 / size.with_lz4 as f64,
        );
    }
}

fn blocks_of<T: Copy>(values: &[T]) -> impl Iterator<Item = &[T]> {
    values.chunks(BLOCK)
}

fn main() {
    println!("e20 — encoding census: size and decode cost of present and absent encodings");
    println!("{ROWS} rows per column, {BLOCK}-row blocks\n", ROWS = N_ORDERS);

    let orders = gen_orders(N_ORDERS, 0x5EED);

    // ---- integer payload column: uniform, no outliers (today's best case)
    let amount = orders.amount.clone();
    census_integer("amount (uniform, benchmark shape)", &amount);

    // ---- integer payload column with a thin outlier tail
    let spiky = amounts_with_outliers(N_ORDERS, 0xA11CE);
    census_integer("amount (0.1% large outliers)", &spiky);

    // ---- low-cardinality columns in both shapes
    let status = orders.status.iter().map(|v| i64::from(*v)).collect::<Vec<_>>();
    census_low_cardinality(
        &format!("status (cycles every {N_STATUS} rows — benchmark shape)"),
        &status,
    );
    let status_clustered = clustered(&status, 0xC1057);
    census_low_cardinality("status (clustered into runs)", &status_clustered);

    let region = orders.region.iter().map(|v| i64::from(*v)).collect::<Vec<_>>();
    census_low_cardinality(
        &format!("region (random over {N_REGIONS} values)"),
        &region,
    );

    let user = orders.user_id.iter().map(|v| i64::from(*v)).collect::<Vec<_>>();
    census_low_cardinality(
        &format!("user_id (random over {N_USERS} values — dictionary is a poor fit)"),
        &user,
    );

    // ---- floats, which PTSEG does not encode at all
    census_float("price (2-decimal money as f64)", &prices(N_ORDERS, 0xBEEF));
    census_float("ratio (genuinely real doubles)", &ratios(N_ORDERS, 0xFEED));
}

fn census_integer(column: &str, values: &[i64]) {
    let raw = values.len() * 8;
    let for_blocks = blocks_of(values).map(encode_for).collect::<Vec<_>>();
    let pfor_blocks = blocks_of(values)
        .map(|b| encode_pfor(b, 0.02))
        .collect::<Vec<_>>();

    let raw_bytes = as_bytes_i64(values);
    let for_bytes: usize = for_blocks.iter().map(ForBlock::bytes).sum();
    let pfor_bytes: usize = pfor_blocks.iter().map(PforBlock::bytes).sum();
    let for_lz4: usize = for_blocks.iter().map(|b| lz4_size(&b.to_bytes())).sum();
    let pfor_lz4: usize = pfor_blocks.iter().map(|b| lz4_size(&b.to_bytes())).sum();
    let widths: Vec<u32> = for_blocks.iter().map(|b| b.width).collect();
    let pfor_widths: Vec<u32> = pfor_blocks.iter().map(|b| b.width).collect();

    report_sizes(
        column,
        raw,
        values.len(),
        &[
            Sizing { name: "plain", encoded: raw, with_lz4: lz4_size(&raw_bytes) },
            Sizing {
                name: "FOR + bitpack (PTSEG today)",
                encoded: for_bytes,
                with_lz4: for_lz4,
            },
            Sizing {
                name: "FOR + bitpack + patched exceptions",
                encoded: pfor_bytes,
                with_lz4: pfor_lz4,
            },
        ],
    );
    println!(
        "  block widths: FOR {}..{} bits, patched {}..{} bits",
        widths.iter().min().unwrap_or(&0),
        widths.iter().max().unwrap_or(&0),
        pfor_widths.iter().min().unwrap_or(&0),
        pfor_widths.iter().max().unwrap_or(&0),
    );

    let expected = checksum_i64(values);
    let mut results = Vec::new();
    results.push(bench("  decode: FOR + bitpack", || {
        let mut scratch = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &for_blocks {
            block.decode_into(&mut scratch, &mut out);
        }
        checksum_i64(&out)
    }));
    // The second layer's real price. PTSEG compresses every encoded block
    // with LZ4, so a cold scan pays an LZ4 decompress before it can unpack
    // anything. BtrBlocks (SIGMOD 2023, §2.1) argues that layer is what
    // makes formats decode slowly; this measures it on our own encoding.
    let compressed_blocks = for_blocks
        .iter()
        .map(|block| {
            let plain = block.to_bytes();
            (lz4_flex::block::compress(&plain), plain.len())
        })
        .collect::<Vec<_>>();
    results.push(bench("  decode: lz4 + FOR + bitpack (PTSEG pipeline)", || {
        let mut scratch = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for (block, (compressed, plain_len)) in for_blocks.iter().zip(&compressed_blocks) {
            let plain = lz4_flex::block::decompress(compressed, *plain_len)
                .expect("block round-trips");
            // Unpack out of the bytes the decompressor just produced, which
            // is the only buffer a real decoder has.
            unpack_from_bytes(&plain[12..], block.width, block.rows, &mut scratch);
            out.extend(scratch.iter().map(|d| block.base.wrapping_add(*d as i64)));
        }
        checksum_i64(&out)
    }));
    results.push(bench("  decode: FOR + patched exceptions", || {
        let mut scratch = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &pfor_blocks {
            block.decode_into(&mut scratch, &mut out);
        }
        checksum_i64(&out)
    }));
    assert_eq!(results[0].checksum, expected, "FOR decode must round-trip");
    check_consistency(&results);
}

fn census_low_cardinality(column: &str, values: &[i64]) {
    let raw = values.len() * 8;
    let dict_blocks = blocks_of(values).map(encode_dict).collect::<Vec<_>>();
    let run_blocks = blocks_of(values).map(encode_run_end).collect::<Vec<_>>();

    let raw_bytes = as_bytes_i64(values);
    let dict_bytes: usize = dict_blocks.iter().map(DictBlock::bytes).sum();
    let run_bytes: usize = run_blocks.iter().map(RunEndBlock::bytes).sum();
    let dict_lz4: usize = dict_blocks.iter().map(|b| lz4_size(&b.to_bytes())).sum();
    let run_lz4: usize = run_blocks.iter().map(|b| lz4_size(&b.to_bytes())).sum();
    let runs: usize = run_blocks.iter().map(RunEndBlock::runs).sum();

    report_sizes(
        column,
        raw,
        values.len(),
        &[
            Sizing { name: "plain", encoded: raw, with_lz4: lz4_size(&raw_bytes) },
            Sizing {
                name: "dictionary codes (PTSEG today)",
                encoded: dict_bytes,
                with_lz4: dict_lz4,
            },
            Sizing {
                name: "run-end over dictionary codes",
                encoded: run_bytes,
                with_lz4: run_lz4,
            },
        ],
    );
    println!(
        "  {runs} runs over {} rows ({:.1} rows/run)",
        values.len(),
        values.len() as f64 / runs as f64
    );

    let expected = checksum_i64(values);
    let mut results = Vec::new();
    results.push(bench("  decode: dictionary codes", || {
        let mut scratch = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &dict_blocks {
            block.decode_into(&mut scratch, &mut out);
        }
        checksum_i64(&out)
    }));
    results.push(bench("  decode: run-end", || {
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &run_blocks {
            block.decode_into(&mut out);
        }
        checksum_i64(&out)
    }));
    assert_eq!(results[0].checksum, expected, "dictionary decode must round-trip");
    check_consistency(&results);

    // Predicate without decoding: the payoff run-end is actually for.
    let wanted = values[0];
    let scanned = bench("  count(status = v): decode then scan", || {
        let mut scratch = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &dict_blocks {
            block.decode_into(&mut scratch, &mut out);
        }
        out.iter().filter(|v| **v == wanted).count() as u64
    });
    let per_run = bench("  count(status = v): per run, no decode", || {
        run_blocks.iter().map(|b| b.count_matching(wanted)).sum()
    });
    check_consistency(&[scanned, per_run]);
}

fn census_float(column: &str, values: &[f64]) {
    let raw = values.len() * 8;
    let raw_bytes = as_bytes_f64(values);
    let pseudo = blocks_of(values)
        .map(encode_pseudodecimal)
        .collect::<Vec<_>>();
    let encodable = pseudo.iter().all(Option::is_some);

    let mut sizes = vec![Sizing {
        name: "plain + lz4 (PTSEG today)",
        encoded: raw,
        with_lz4: lz4_size(&raw_bytes),
    }];
    if encodable {
        let bytes: usize = pseudo
            .iter()
            .map(|b| b.as_ref().expect("encodable").bytes())
            .sum();
        let compressed: usize = pseudo
            .iter()
            .map(|b| lz4_size(&b.as_ref().expect("encodable").to_bytes()))
            .sum();
        sizes.push(Sizing {
            name: "pseudodecimal (mantissa + exponent)",
            encoded: bytes,
            with_lz4: compressed,
        });
    }
    report_sizes(column, raw, values.len(), &sizes);
    if !encodable {
        println!("  pseudodecimal: rejected — values are not decimal-like");
        return;
    }

    let expected = checksum_f64(values);
    let decoded = bench("  decode: pseudodecimal", || {
        let mut scratch = Vec::new();
        let mut ints = Vec::new();
        let mut out = Vec::with_capacity(N_ORDERS);
        for block in &pseudo {
            block
                .as_ref()
                .expect("encodable")
                .decode_into(&mut scratch, &mut ints, &mut out);
        }
        checksum_f64(&out)
    });
    assert_eq!(
        decoded.checksum, expected,
        "pseudodecimal must reconstruct every double exactly"
    );
}
