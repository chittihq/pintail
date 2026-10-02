//! Bit-unpacking: fixed-width values packed LSB-first into a byte stream,
//! widened to one `u64` each.
//!
//! Value `i` of a `width`-bit stream occupies bits `i * width ..` of the
//! stream, bit `b` being bit `b % 8` of byte `b / 8`. Eight values fill
//! exactly `width` bytes, so every run of eight starts on a byte and the
//! position of each value inside its run depends on the width alone. The
//! AVX2 kernel uses that: four values a step, two to a 128-bit lane, each
//! lane fed the sixteen bytes from the byte its first value starts in; one
//! constant byte shuffle moves each value's bytes into its 64-bit lane, one
//! per-lane shift aligns it and one mask trims it. Widths 1 to 57 fit (7
//! bits of offset plus the value in a lane); the portable decode, a 16-byte
//! window a value, serves the rest.

/// Values in one run: the unit a vector kernel decodes at once.
const RUN: usize = 8;

/// Bytes one run's load reads.
const WINDOW: usize = 64;

/// Widest value the AVX2 kernel decodes: 7 bits of offset plus the value
/// must fit a 64-bit lane.
pub const AVX2_WIDEST: u32 = 57;

const fn width_mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    }
}

#[inline(always)]
fn check(width: u32, bytes: &[u8], values: usize) {
    assert!(width <= 64, "bit width {width} exceeds 64");
    let needed = values
        .checked_mul(width as usize)
        .expect("packed length fits usize")
        .div_ceil(8);
    assert!(
        bytes.len() >= needed,
        "{values} values of {width} bits need {needed} bytes, payload holds {}",
        bytes.len()
    );
}

/// Decodes `out.len()` values of `width` bits from the start of `bytes`,
/// each plus `base` (wrapping): a block stores its values less their
/// minimum, and the add is free while the value is in a register.
///
/// # Panics
///
/// When `width` exceeds 64 or `bytes` holds fewer bits than the values need.
#[inline(always)]
pub fn unpack_u64(width: u32, bytes: &[u8], base: u64, out: &mut [u64]) {
    check(width, bytes, out.len());
    unpack_portable(width, bytes, base, out);
}

/// The portable decode.
#[inline(always)]
fn unpack_portable(width: u32, bytes: &[u8], base: u64, out: &mut [u64]) {
    if width == 0 {
        out.fill(base);
        return;
    }
    let mask = width_mask(width);
    for (index, value) in out.iter_mut().enumerate() {
        let bit = index * width as usize;
        let (at, shift) = (bit / 8, bit % 8);
        let window: [u8; 16] = if let Some(window) = bytes.get(at..at + 16) {
            window.try_into().expect("sixteen bytes")
        } else {
            let mut padded = [0_u8; 16];
            let rest = &bytes[at..];
            padded[..rest.len()].copy_from_slice(rest);
            padded
        };
        #[allow(clippy::cast_possible_truncation)]
        let low = (u128::from_le_bytes(window) >> shift) as u64;
        *value = (low & mask).wrapping_add(base);
    }
}

// Decodes each run of eight values from the 64 bytes starting at the run's
// first byte; the last run may be short.
//
// Two loops. The first takes every whole run whose 64 bytes lie inside the
// payload - all but the last few - and holds nothing but the decode: no
// call, so the kernel's constants stay in registers. The second pads the
// window past the payload's end with zeros for what is left.
//
// A macro rather than a function taking a closure: a closure is a function
// of its own, compiled for the vector instruction set only when it happens
// to be inlined into the kernel, and one with several call sites was not -
// every intrinsic in it then became a call.
#[cfg(target_arch = "x86_64")]
macro_rules! for_each_run {
    ($width:expr, $bytes:expr, $out:expr, |$window:ident| $decode:expr) => {{
        let step = $width as usize;
        let whole_runs = $out.len() / RUN;
        let direct = match $bytes.len().checked_sub(WINDOW) {
            Some(slack) => (slack / step + 1).min(whole_runs),
            None => 0,
        };
        let (head, tail) = $out.split_at_mut(direct * RUN);
        for (index, run) in head.chunks_exact_mut(RUN).enumerate() {
            let start = index * step;
            let $window: &[u8; WINDOW] = $bytes[start..start + WINDOW]
                .try_into()
                .expect("window bytes");
            let values: [u64; RUN] = $decode;
            run.copy_from_slice(&values);
        }
        for (index, run) in tail.chunks_mut(RUN).enumerate() {
            let padded = padded_window($bytes, (direct + index) * step);
            let $window: &[u8; WINDOW] = &padded;
            let values: [u64; RUN] = $decode;
            let held = run.len();
            run.copy_from_slice(&values[..held]);
        }
    }};
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn padded_window(bytes: &[u8], start: usize) -> [u8; WINDOW] {
    let mut padded = [0_u8; WINDOW];
    let rest = bytes.get(start..).unwrap_or(&[]);
    let held = rest.len().min(WINDOW);
    padded[..held].copy_from_slice(&rest[..held]);
    padded
}

/// [`unpack_u64`] on AVX2, for widths up to [`AVX2_WIDEST`]; wider values
/// take the portable decode.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn unpack_u64_avx2(
    simd: pulp::x86::V3,
    width: u32,
    bytes: &[u8],
    base: u64,
    out: &mut [u64],
) {
    use core::arch::x86_64::__m256i;
    check(width, bytes, out.len());
    if width == 0 || width > AVX2_WIDEST {
        unpack_portable(width, bytes, base, out);
        return;
    }
    // A run is four pairs of values, each pair in one 128-bit lane fed by
    // the sixteen bytes from the byte its first value starts in.
    let mut offsets = [0_usize; 4];
    let mut shuffles = [[0_u8; 16]; 4];
    let mut shifts = [[0_u64; 2]; 4];
    for pair in 0..4 {
        let bit = pair * 2 * width as usize;
        offsets[pair] = bit / 8;
        for half in 0..2 {
            let within = bit % 8 + half * width as usize;
            for byte in 0..8 {
                shuffles[pair][half * 8 + byte] = u8::try_from(within / 8 + byte).expect("byte");
            }
            shifts[pair][half] = (within % 8) as u64;
        }
    }
    let shuffle: [__m256i; 2] = [
        pulp::cast([shuffles[0], shuffles[1]]),
        pulp::cast([shuffles[2], shuffles[3]]),
    ];
    let shift: [__m256i; 2] = [
        pulp::cast([shifts[0], shifts[1]]),
        pulp::cast([shifts[2], shifts[3]]),
    ];
    let mask = simd.avx._mm256_set1_epi64x(width_mask(width).cast_signed());
    let base = simd.avx._mm256_set1_epi64x(base.cast_signed());
    // The last pair starts at most 42 bytes in, so its sixteen bytes end
    // inside the window.
    assert!(offsets[3] + 16 <= WINDOW);
    for_each_run!(width, bytes, out, |window| {
        let low = half_run(
            simd,
            window,
            [offsets[0], offsets[1]],
            [shuffle[0], shift[0], mask, base],
        );
        let high = half_run(
            simd,
            window,
            [offsets[2], offsets[3]],
            [shuffle[1], shift[1], mask, base],
        );
        pulp::cast([low, high])
    });
}

/// Four values of a run on AVX2: two pairs, each from its own sixteen
/// bytes of the window. Each lane loads into its own 128-bit register and
/// the two are joined there: assembling the 32 bytes in memory first would
/// make the wide load wait on two narrower stores.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn half_run(
    simd: pulp::x86::V3,
    window: &[u8; WINDOW],
    offsets: [usize; 2],
    [shuffle, shift, mask, base]: [core::arch::x86_64::__m256i; 4],
) -> [u64; 4] {
    let low: [u8; 16] = window[offsets[0]..offsets[0] + 16]
        .try_into()
        .expect("sixteen bytes");
    let high: [u8; 16] = window[offsets[1]..offsets[1] + 16]
        .try_into()
        .expect("sixteen bytes");
    let loaded = simd.avx._mm256_set_m128i(pulp::cast(high), pulp::cast(low));
    let placed = simd.avx2._mm256_shuffle_epi8(loaded, shuffle);
    let aligned = simd.avx2._mm256_srlv_epi64(placed, shift);
    let value = simd.avx2._mm256_and_si256(aligned, mask);
    pulp::cast(simd.avx2._mm256_add_epi64(value, base))
}
