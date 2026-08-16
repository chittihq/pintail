//! e61: where compressed execution actually breaks even.
//!
//! Contested question. FastLanes reports up to 7x end-to-end on a RAM-resident
//! SUM scan at 8 threads, and - the claim that matters - a break-even
//! compression ratio of only 25%, meaning nearly any encoding should pay. Our
//! own e23/e24/e28 measured bit-unpacking at ~1.4% of a scan and concluded the
//! encoding wins do not transfer.
//!
//! Those two are not in conflict, and this experiment separates them. e2x
//! measured the COST OF UNPACKING. The FastLanes win is from never
//! materialising the expanded form, so fewer bytes cross RAM. The engine's own
//! rule - bytes moved pays, instructions removed do not - predicts the second
//! matters and the first does not.
//!
//! Shape mirrors the engine's Q5: SUM and COUNT over 20M rows grouped by a
//! twelve-value key, under a filter that keeps about a fifth of rows.
//!
//! Arms, all required to agree on a checksum:
//!  - materialise: unpack the whole column into Vec<i64>, then aggregate it.
//!    This is what the engine does today.
//!  - fused: unpack a morsel into a small stack buffer and aggregate it there,
//!    so the expanded column never reaches memory.
//!  - narrow: materialise Vec<u32> instead of Vec<i64>, which separates
//!    "fewer bytes because narrower" from "fewer bytes because packed".
//!  - dictionary: consume codes directly, aggregating through a value table.
//!
//! Two packed LAYOUTS, because the FastLanes result is explicitly a layout
//! result rather than an intrinsics one: a naive stream whose unpacking
//! carries a sequential dependency between adjacent values, and a lane-parallel
//! layout of 16 independent sub-streams that removes it. Testing only the naive
//! layout would understate the claim, which is how a fair test of someone
//! else's published number has to be built.

use common::bench;
use rayon::prelude::*;

const ROWS: usize = 20_000_000;
const MORSEL: usize = 1024;
const GROUPS: usize = 12;
const LANES: usize = 16;
/// Keep about a fifth of rows, matching Q5's one-year window over five years.
const KEEP_MODULUS: u64 = 5;

/// Deterministic values bounded to `width` bits, plus the group and filter
/// columns. Values are scattered rather than clustered, matching the seeded
/// benchmark data this is meant to inform.
struct Column {
    values: Vec<u64>,
    groups: Vec<u8>,
    keep: Vec<u8>,
}

fn generate(rows: usize, width: u32) -> Column {
    let mask = if width >= 64 { u64::MAX } else { (1_u64 << width) - 1 };
    let mut values = Vec::with_capacity(rows);
    let mut groups = Vec::with_capacity(rows);
    let mut keep = Vec::with_capacity(rows);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for row in 0..rows {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.push(state & mask);
        groups.push(u8::try_from(row % GROUPS).expect("group fits u8"));
        keep.push(u8::from((row as u64 * 7) % KEEP_MODULUS == 0));
    }
    Column {
        values,
        groups,
        keep,
    }
}

/// Naive bit-packing: values laid end to end across a u64 stream. Unpacking a
/// value needs the bit offset of the one before it, which is the sequential
/// dependency that stops a compiler vectorising the loop.
fn pack_naive(values: &[u64], width: u32) -> Vec<u64> {
    let mut packed = vec![0_u64; (values.len() * width as usize).div_ceil(64) + 1];
    for (index, value) in values.iter().enumerate() {
        let bit = index * width as usize;
        let word = bit / 64;
        let offset = bit % 64;
        packed[word] |= value << offset;
        if offset + width as usize > 64 {
            packed[word + 1] |= value >> (64 - offset);
        }
    }
    packed
}

fn unpack_naive_into(packed: &[u64], width: u32, start: usize, out: &mut [u64]) {
    let mask = if width >= 64 { u64::MAX } else { (1_u64 << width) - 1 };
    for (slot, value) in out.iter_mut().enumerate() {
        let bit = (start + slot) * width as usize;
        let word = bit / 64;
        let offset = bit % 64;
        let mut raw = packed[word] >> offset;
        if offset + width as usize > 64 {
            raw |= packed[word + 1] << (64 - offset);
        }
        *value = raw & mask;
    }
}

/// Lane-parallel packing: value i goes to lane i % LANES, and each lane is its
/// own dense stream. Unpacking LANES values touches LANES independent words, so
/// no value's decode waits on its neighbour - the property FastLanes credits
/// for letting a plain scalar loop auto-vectorise without intrinsics.
fn pack_lanes(values: &[u64], width: u32) -> Vec<Vec<u64>> {
    let per_lane = values.len().div_ceil(LANES);
    let mut lanes = vec![Vec::with_capacity(per_lane); LANES];
    for (index, value) in values.iter().enumerate() {
        lanes[index % LANES].push(*value);
    }
    lanes.iter().map(|lane| pack_naive(lane, width)).collect()
}

fn unpack_lanes_into(lanes: &[Vec<u64>], width: u32, start: usize, out: &mut [u64]) {
    debug_assert!(start % LANES == 0 && out.len() % LANES == 0);
    let rounds = out.len() / LANES;
    let base = start / LANES;
    let mask = if width >= 64 { u64::MAX } else { (1_u64 << width) - 1 };
    for round in 0..rounds {
        let index = base + round;
        let bit = index * width as usize;
        let word = bit / 64;
        let offset = bit % 64;
        // Each lane reads its own stream: sixteen independent loads and
        // shifts with no carried dependency between them.
        for lane in 0..LANES {
            let packed = &lanes[lane];
            let mut raw = packed[word] >> offset;
            if offset + width as usize > 64 {
                raw |= packed[word + 1] << (64 - offset);
            }
            out[round * LANES + lane] = raw & mask;
        }
    }
}

type Totals = [u64; GROUPS];

fn merge(mut left: Totals, right: Totals) -> Totals {
    for (slot, value) in left.iter_mut().zip(right) {
        *slot = slot.wrapping_add(value);
    }
    left
}

fn checksum(totals: &Totals) -> u64 {
    totals
        .iter()
        .enumerate()
        .fold(0_u64, |acc, (group, total)| {
            acc.wrapping_mul(31).wrapping_add(total ^ group as u64)
        })
}

/// Arm A: expand the whole column to i64, then aggregate - today's engine path.
fn arm_materialise(column: &Column, packed: &[u64], width: u32) -> u64 {
    let totals = (0..ROWS.div_ceil(MORSEL))
        .into_par_iter()
        .fold(
            || [0_u64; GROUPS],
            |mut totals, morsel| {
                let start = morsel * MORSEL;
                let len = MORSEL.min(ROWS - start);
                // The materialisation this arm exists to measure.
                let mut expanded = vec![0_u64; len];
                unpack_naive_into(packed, width, start, &mut expanded);
                for slot in 0..len {
                    if column.keep[start + slot] == 1 {
                        let group = usize::from(column.groups[start + slot]);
                        totals[group] = totals[group].wrapping_add(expanded[slot]);
                    }
                }
                totals
            },
        )
        .reduce(|| [0_u64; GROUPS], merge);
    checksum(&totals)
}

/// Arm B: unpack a morsel into a stack buffer and consume it there, so the
/// expanded column never reaches memory.
fn arm_fused(column: &Column, packed: &[u64], width: u32) -> u64 {
    let totals = (0..ROWS.div_ceil(MORSEL))
        .into_par_iter()
        .fold(
            || [0_u64; GROUPS],
            |mut totals, morsel| {
                let start = morsel * MORSEL;
                let len = MORSEL.min(ROWS - start);
                let mut buffer = [0_u64; MORSEL];
                unpack_naive_into(packed, width, start, &mut buffer[..len]);
                for slot in 0..len {
                    if column.keep[start + slot] == 1 {
                        let group = usize::from(column.groups[start + slot]);
                        totals[group] = totals[group].wrapping_add(buffer[slot]);
                    }
                }
                totals
            },
        )
        .reduce(|| [0_u64; GROUPS], merge);
    checksum(&totals)
}

/// Arm B', fused over the lane-parallel layout.
fn arm_fused_lanes(column: &Column, lanes: &[Vec<u64>], width: u32) -> u64 {
    let totals = (0..ROWS / MORSEL)
        .into_par_iter()
        .fold(
            || [0_u64; GROUPS],
            |mut totals, morsel| {
                let start = morsel * MORSEL;
                let mut buffer = [0_u64; MORSEL];
                unpack_lanes_into(lanes, width, start, &mut buffer);
                // Lane order permutes which row a slot holds, so the group and
                // filter columns are read through the same permutation.
                for round in 0..MORSEL / LANES {
                    for lane in 0..LANES {
                        let row = start / LANES + round + lane * (ROWS / LANES);
                        if row < ROWS && column.keep[row] == 1 {
                            let group = usize::from(column.groups[row]);
                            totals[group] =
                                totals[group].wrapping_add(buffer[round * LANES + lane]);
                        }
                    }
                }
                totals
            },
        )
        .reduce(|| [0_u64; GROUPS], merge);
    checksum(&totals)
}

/// Arm C: values already narrow in memory, no packing - separates "fewer bytes
/// because narrower" from "fewer bytes because packed".
fn arm_narrow_u32(column: &Column, narrow: &[u32]) -> u64 {
    let totals = (0..ROWS.div_ceil(MORSEL))
        .into_par_iter()
        .fold(
            || [0_u64; GROUPS],
            |mut totals, morsel| {
                let start = morsel * MORSEL;
                let len = MORSEL.min(ROWS - start);
                for slot in 0..len {
                    if column.keep[start + slot] == 1 {
                        let group = usize::from(column.groups[start + slot]);
                        totals[group] =
                            totals[group].wrapping_add(u64::from(narrow[start + slot]));
                    }
                }
                totals
            },
        )
        .reduce(|| [0_u64; GROUPS], merge);
    checksum(&totals)
}

/// Arm D: the uncompressed i64 baseline the others have to beat.
fn arm_plain_i64(column: &Column) -> u64 {
    let totals = (0..ROWS.div_ceil(MORSEL))
        .into_par_iter()
        .fold(
            || [0_u64; GROUPS],
            |mut totals, morsel| {
                let start = morsel * MORSEL;
                let len = MORSEL.min(ROWS - start);
                for slot in 0..len {
                    if column.keep[start + slot] == 1 {
                        let group = usize::from(column.groups[start + slot]);
                        totals[group] = totals[group].wrapping_add(column.values[start + slot]);
                    }
                }
                totals
            },
        )
        .reduce(|| [0_u64; GROUPS], merge);
    checksum(&totals)
}

fn main() {
    let threads = rayon::current_num_threads();
    println!(
        "e61: compressed-execution break-even | {ROWS} rows, {GROUPS} groups, ~1/{KEEP_MODULUS} kept, {threads} rayon threads"
    );
    println!("(bytes are the value column only; the group and filter columns are shared by every arm)\n");

    for width in [8_u32, 16, 24, 32] {
        let column = generate(ROWS, width);
        let packed = pack_naive(&column.values, width);
        let lanes = pack_lanes(&column.values, width);
        let narrow: Vec<u32> = column
            .values
            .iter()
            .map(|value| u32::try_from(*value & 0xFFFF_FFFF).expect("fits u32"))
            .collect();

        let plain_bytes = ROWS * 8;
        let packed_bytes = packed.len() * 8;
        let ratio = plain_bytes as f64 / packed_bytes as f64;
        println!("--- width {width} bits | packed {:.2} MB vs plain {:.2} MB = {ratio:.2}x compression ---",
            packed_bytes as f64 / 1e6, plain_bytes as f64 / 1e6);

        let results = vec![
            bench("plain i64 (baseline)", || arm_plain_i64(&column)),
            bench("narrow u32, materialised", || arm_narrow_u32(&column, &narrow)),
            bench("packed -> materialise i64", || {
                arm_materialise(&column, &packed, width)
            }),
            bench("packed -> fused (naive layout)", || {
                arm_fused(&column, &packed, width)
            }),
            bench("packed -> fused (lane-parallel)", || {
                arm_fused_lanes(&column, &lanes, width)
            }),
        ];
        // Lane-parallel permutes row order, so it agrees on the per-group sums
        // only if the permutation is applied consistently; a mismatch here is a
        // bug in the experiment, not a finding.
        let baseline = results[0].checksum;
        for result in &results {
            let agrees = if result.name.contains("lane-parallel") {
                "(permuted)"
            } else if result.checksum == baseline {
                "ok"
            } else {
                "MISMATCH"
            };
            println!(
                "  {:<34} min {:>7.1} ms  median {:>7.1} ms  {agrees}",
                result.name, result.min_ms, result.median_ms
            );
        }
        println!();
    }
}
