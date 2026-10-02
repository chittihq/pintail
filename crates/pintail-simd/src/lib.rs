//! Safe vector kernels for the executor, dispatched at runtime by CPU.
//!
//! Two kinds of kernel live here. Most are ordinary safe Rust shaped for the
//! auto-vectorizer ([`portable`]). The ones the auto-vectorizer lowers
//! badly - compare-to-bitmask, mask expansion, the exact `i64` sum - also
//! have hand-written AVX2 bodies built from the dispatcher's safe intrinsic
//! wrappers. Each public function picks, once per call, the copy compiled
//! for the running CPU: AVX2 (x86-64-v3) when present, else the build's
//! baseline. AVX-512 is opt-in (`PINTAIL_SIMD=avx512`) because it measured
//! slower on the reference machine. The dispatcher, a dependency, owns the
//! one `#[target_feature]` call this needs, so Pintail's crates keep
//! `unsafe_code = "forbid"` and one release binary still runs on every
//! x86-64 CPU. See `docs/decisions.md`, "SIMD route".
//!
//! A third kind is written for one instruction set outright and has no
//! auto-vectorized form worth having: bit-unpacking ([`unpack_u64`]) and
//! dictionary-code translation ([`translate_codes`]) have AVX2 bodies.
//!
//! Masks use the executor's selection-word layout: bit `i % 64` of word
//! `i / 64` is row `i`, and bits past the last row are zero.
//!
//! `PINTAIL_SIMD` is a diagnostic, read once: `off` (or `portable`,
//! `baseline`) runs every kernel at the baseline target - the same binary,
//! for A/B measurement and for ruling the dispatch out of a wrong answer;
//! `avx2` is the default spelled out; `avx512` runs the auto-vectorized
//! kernels at AVX-512.

// `#[inline(always)]` is the mechanism here, not a hint: a kernel is only
// compiled for AVX2 when it is inlined into the dispatcher's target-feature
// function, and LLVM's own heuristics declined the larger kernels.
#![allow(clippy::inline_always)]

#[cfg(target_arch = "x86_64")]
mod avx2;
pub mod codes;
pub mod portable;
pub mod unpack;

use std::sync::LazyLock;

/// Instruction set the kernels dispatch to on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// The build's baseline target (SSE2 on generic x86-64).
    Baseline,
    /// AVX2 + FMA + BMI2 (x86-64-v3).
    Avx2,
    /// AVX-512 F/BW/CD/DQ/VL (x86-64-v4).
    Avx512,
}

impl Level {
    /// Short name for logs and profiles.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Avx2 => "avx2",
            Self::Avx512 => "avx512",
        }
    }
}

#[derive(Clone, Copy)]
enum Dispatch {
    Baseline,
    #[cfg(target_arch = "x86_64")]
    Avx2(pulp::x86::V3),
    #[cfg(target_arch = "x86_64")]
    Avx512(pulp::x86::V4, pulp::x86::V3),
    #[cfg(not(target_arch = "x86_64"))]
    Native(pulp::Arch),
}

fn cap() -> String {
    std::env::var("PINTAIL_SIMD")
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn capped_to_baseline(cap: &str) -> bool {
    matches!(cap, "off" | "baseline" | "portable")
}

static DISPATCH: LazyLock<Dispatch> = LazyLock::new(|| select(cap().as_str()));

#[cfg(target_arch = "x86_64")]
fn select(cap: &str) -> Dispatch {
    if capped_to_baseline(cap) {
        return Dispatch::Baseline;
    }
    let Some(v3) = pulp::x86::V3::try_new() else {
        return Dispatch::Baseline;
    };
    // AVX-512 is opt-in: on the reference machine its auto-vectorized
    // copies streamed from memory three to five times slower than AVX2's.
    if cap == "avx512"
        && let Some(v4) = pulp::x86::V4::try_new()
    {
        return Dispatch::Avx512(v4, v3);
    }
    Dispatch::Avx2(v3)
}

#[cfg(not(target_arch = "x86_64"))]
fn select(cap: &str) -> Dispatch {
    if capped_to_baseline(cap) {
        Dispatch::Baseline
    } else {
        Dispatch::Native(pulp::Arch::new())
    }
}

/// The instruction set kernels run at in this process.
#[must_use]
pub fn level() -> Level {
    match *DISPATCH {
        #[cfg(target_arch = "x86_64")]
        Dispatch::Avx2(_) => Level::Avx2,
        #[cfg(target_arch = "x86_64")]
        Dispatch::Avx512(..) => Level::Avx512,
        _ => Level::Baseline,
    }
}

// Each public kernel becomes a small struct implementing the dispatcher's
// trait, whose `#[inline(always)]` method calls the `#[inline(always)]`
// portable kernel directly. That direct call is what gets the kernel inlined
// into - and vectorized for - each target-feature copy. Handing the
// dispatcher a closure or a function item instead left an out-of-line call
// (the closure body, or the function item's `FnOnce` shim) compiled for the
// baseline, which silently dispatched to the scalar build.
macro_rules! dispatched {
    ($(
        $(#[$meta:meta])*
        pub fn $name:ident<$($lt:lifetime),* $(; $t:ident: $bound:path)?>(
            $($arg:ident: $ty:ty),* $(,)?
        ) -> $ret:ty => $kernel:path;
    )+) => {$(
        $(#[$meta])*
        #[allow(clippy::needless_lifetimes, clippy::unused_unit)]
        pub fn $name<$($lt),* $(, $t: $bound)?>($($arg: $ty),*) -> $ret {
            struct Kernel<$($lt),* $(, $t)?> {
                $($arg: $ty),*
            }

            impl<$($lt),* $(, $t: $bound)?> pulp::WithSimd for Kernel<$($lt),* $(, $t)?> {
                type Output = $ret;

                #[inline(always)]
                fn with_simd<S: pulp::Simd>(self, _simd: S) -> $ret {
                    $kernel($(self.$arg),*)
                }
            }

            run(Kernel { $($arg),* })
        }
    )+};
}

#[inline]
fn run<Op: pulp::WithSimd>(op: Op) -> Op::Output {
    match *DISPATCH {
        Dispatch::Baseline => op.with_simd(pulp::Scalar::new()),
        #[cfg(target_arch = "x86_64")]
        Dispatch::Avx2(simd) => pulp::Simd::vectorize(simd, op),
        #[cfg(target_arch = "x86_64")]
        Dispatch::Avx512(simd, _) => pulp::Simd::vectorize(simd, op),
        #[cfg(not(target_arch = "x86_64"))]
        Dispatch::Native(arch) => arch.dispatch(op),
    }
}

/// The AVX2 token when dispatch runs at AVX2 or above.
#[cfg(target_arch = "x86_64")]
#[inline]
fn avx2() -> Option<pulp::x86::V3> {
    match *DISPATCH {
        Dispatch::Avx2(v3) | Dispatch::Avx512(_, v3) => Some(v3),
        Dispatch::Baseline => None,
    }
}

// The comparison kernels have hand-written AVX2 bodies (see `avx2.rs`); the
// portable kernel serves the baseline and other architectures.
macro_rules! dispatched_avx2 {
    ($(
        $(#[$meta:meta])*
        pub fn $name:ident<$($lt:lifetime),*>(
            $($arg:ident: $ty:ty),* $(,)?
        ) => $kernel:path, $fast:path;
    )+) => {$(
        $(#[$meta])*
        #[allow(clippy::needless_lifetimes)]
        pub fn $name<$($lt),*>($($arg: $ty),*) {
            #[cfg(target_arch = "x86_64")]
            if let Some(simd) = avx2() {
                struct Fast<$($lt),*> {
                    simd: pulp::x86::V3,
                    $($arg: $ty),*
                }

                impl<$($lt),*> pulp::WithSimd for Fast<$($lt),*> {
                    type Output = ();

                    #[inline(always)]
                    fn with_simd<S: pulp::Simd>(self, _simd: S) {
                        $fast(self.simd, $(self.$arg),*);
                    }
                }

                pulp::Simd::vectorize(simd, Fast { simd, $($arg),* });
                return;
            }
            $kernel($($arg),*);
        }
    )+};
}

// Value-returning kernels with an AVX2 body of their own.
macro_rules! dispatched_avx2_value {
    ($(
        $(#[$meta:meta])*
        pub fn $name:ident(values: &[$ty:ty]) -> $ret:ty => $kernel:path, $fast:path;
    )+) => {$(
        $(#[$meta])*
        #[must_use]
        pub fn $name(values: &[$ty]) -> $ret {
            #[cfg(target_arch = "x86_64")]
            if let Some(simd) = avx2() {
                struct Fast<'a> {
                    simd: pulp::x86::V3,
                    values: &'a [$ty],
                }

                impl pulp::WithSimd for Fast<'_> {
                    type Output = $ret;

                    #[inline(always)]
                    fn with_simd<S: pulp::Simd>(self, _simd: S) -> $ret {
                        $fast(self.simd, self.values)
                    }
                }

                return pulp::Simd::vectorize(simd, Fast { simd, values });
            }
            $kernel(values)
        }
    )+};
}

dispatched_avx2_value! {
    /// Exact sum as `i128`; never overflows. See [`portable::sum_i64`].
    pub fn sum_i64(values: &[i64]) -> i128 => portable::sum_i64, avx2::sum_i64;

    /// Smallest value, `None` when empty.
    pub fn min_i64(values: &[i64]) -> Option<i64> => portable::min_i64, avx2::min_i64;

    /// Largest value, `None` when empty.
    pub fn max_i64(values: &[i64]) -> Option<i64> => portable::max_i64, avx2::max_i64;
}

/// Comparison applied between each value and a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `value = constant`
    Eq,
    /// `value <> constant`
    Ne,
    /// `value < constant`
    Lt,
    /// `value <= constant`
    Le,
    /// `value > constant`
    Gt,
    /// `value >= constant`
    Ge,
}

dispatched! {
    /// Reassociated float sum; see [`portable::sum_f64`] for the order.
    pub fn sum_f64<'a>(values: &'a [f64]) -> f64 => portable::sum_f64;

    /// Smallest value of NaN-free input, `None` when empty.
    pub fn min_f64<'a>(values: &'a [f64]) -> Option<f64> => portable::min_f64;

    /// Largest value of NaN-free input, `None` when empty.
    pub fn max_f64<'a>(values: &'a [f64]) -> Option<f64> => portable::max_f64;

    /// Appends `source[index]` for each index (dictionary decode or
    /// selection compaction).
    ///
    /// # Panics
    ///
    /// When an index is out of bounds for `source`.
    pub fn gather<'a, 'b, 'c; T: Copy>(source: &'a [T], indices: &'b [u32], out: &'c mut Vec<T>) -> ()
        => portable::gather;
}

dispatched! {
    /// Exact sum of Decimal128 payloads, `None` on overflow.
    pub fn sum_i128<'a>(values: &'a [i128]) -> Option<i128> => portable::sum_i128;

    /// Grouped exact sum: `sums[groups[i]] += values[i]` as `i128`, in row
    /// order (see [`portable::grouped`]).
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn sum_by_group_i64<'a, 'b, 'c>(values: &'a [i64], groups: &'b [u32], sums: &'c mut [i128]) -> ()
        => portable::sum_by_group_i64;

    /// Grouped Decimal128 sum (wrapping; see [`portable::sum_by_group_i128`]).
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn sum_by_group_i128<'a, 'b, 'c>(values: &'a [i128], groups: &'b [u32], sums: &'c mut [i128]) -> ()
        => portable::sum_by_group_i128;

    /// Grouped float sum in strict row order per group.
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn sum_by_group_f64<'a, 'b, 'c>(values: &'a [f64], groups: &'b [u32], sums: &'c mut [f64]) -> ()
        => portable::sum_by_group_f64;

    /// AVG's state in one pass: grouped exact sum and row count.
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn sum_count_by_group_i64<'a, 'b, 'c, 'd>(
        values: &'a [i64],
        groups: &'b [u32],
        sums: &'c mut [i128],
        counts: &'d mut [u64],
    ) -> () => portable::sum_count_by_group_i64;

    /// Grouped row count: `counts[group] += 1`.
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds.
    pub fn count_by_group<'a, 'b>(groups: &'a [u32], counts: &'b mut [u64]) -> ()
        => portable::count_by_group;

    /// Grouped minimum (start the slice at `i64::MAX`).
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn min_by_group_i64<'a, 'b, 'c>(values: &'a [i64], groups: &'b [u32], minimums: &'c mut [i64]) -> ()
        => portable::min_by_group_i64;

    /// Grouped maximum (start the slice at `i64::MIN`).
    ///
    /// # Panics
    ///
    /// When a group index is out of bounds or the slices differ in length.
    pub fn max_by_group_i64<'a, 'b, 'c>(values: &'a [i64], groups: &'b [u32], maximums: &'c mut [i64]) -> ()
        => portable::max_by_group_i64;
}

dispatched_avx2! {
    /// Packs per-row flags into mask words (`bools.len().div_ceil(64)`).
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn pack_bools<'a, 'b>(bools: &'a [bool], out: &'b mut [u64])
        => portable::pack_bools, avx2::pack_bools;

    /// Appends the index of every set mask bit below `len` to `out`.
    ///
    /// # Panics
    ///
    /// When `words` is shorter than `len.div_ceil(64)`.
    pub fn mask_to_indices<'a, 'b>(words: &'a [u64], len: usize, out: &'b mut Vec<u32>)
        => portable::mask_to_indices, avx2::mask_to_indices;

    /// Writes `value <op> constant` for each value as mask bits into `out`
    /// (`values.len().div_ceil(64)` words).
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn compare_i64<'a, 'b>(values: &'a [i64], op: CmpOp, constant: i64, out: &'b mut [u64])
        => portable::compare_i64, avx2::compare_i64;

    /// [`compare_i64`] over `i32` values.
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn compare_i32<'a, 'b>(values: &'a [i32], op: CmpOp, constant: i32, out: &'b mut [u64])
        => portable::compare_i32, avx2::compare_i32;

    /// [`compare_i64`] over `u32` values (dictionary codes).
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn compare_u32<'a, 'b>(values: &'a [u32], op: CmpOp, constant: u32, out: &'b mut [u64])
        => portable::compare_u32, avx2::compare_u32;

    /// Writes `low <= value <= high` for each value as mask bits into
    /// `out`. An empty range (`low > high`) selects nothing.
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn between_i64<'a, 'b>(values: &'a [i64], low: i64, high: i64, out: &'b mut [u64])
        => portable::between_i64, avx2::between_i64;

    /// [`between_i64`] over `i32` values.
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn between_i32<'a, 'b>(values: &'a [i32], low: i32, high: i32, out: &'b mut [u64])
        => portable::between_i32, avx2::between_i32;

    /// [`between_i64`] over `u32` values (dictionary codes).
    ///
    /// # Panics
    ///
    /// When `out` is too short.
    pub fn between_u32<'a, 'b>(values: &'a [u32], low: u32, high: u32, out: &'b mut [u64])
        => portable::between_u32, avx2::between_u32;
}

/// The instruction set [`unpack_u64`] decodes `width`-bit values at in
/// this process. A caller with a decode of its own specialised per width
/// keeps it where this answers [`Level::Baseline`].
#[must_use]
pub fn unpack_level(width: u32) -> Level {
    #[cfg(target_arch = "x86_64")]
    if (1..=unpack::AVX2_WIDEST).contains(&width) && avx2().is_some() {
        return Level::Avx2;
    }
    let _ = width;
    Level::Baseline
}

/// Decodes `out.len()` values of `width` bits, packed LSB-first from the
/// start of `bytes`, one per `u64` (see [`unpack`]), each plus `base`
/// (wrapping). `bytes` may run past the values; the kernels read only the
/// bits the values occupy.
///
/// # Panics
///
/// When `width` exceeds 64 or `bytes` holds fewer bits than the values need.
pub fn unpack_u64(width: u32, bytes: &[u8], base: u64, out: &mut [u64]) {
    let ran = unpack_u64_at(unpack_level(width), width, bytes, base, out);
    debug_assert!(ran, "the dispatched level is one the CPU has");
}

/// [`unpack_u64`] into signed values: the same bits, `base` added in two's
/// complement.
///
/// # Panics
///
/// As [`unpack_u64`].
pub fn unpack_i64(width: u32, bytes: &[u8], base: i64, out: &mut [i64]) {
    unpack_u64(
        width,
        bytes,
        base.cast_unsigned(),
        pulp::bytemuck::cast_slice_mut(out),
    );
}

/// [`unpack_u64`] at a named level, for tests and measurement: `false`,
/// with `out` untouched, when this CPU lacks the level or the level has no
/// kernel of its own.
///
/// # Panics
///
/// As [`unpack_u64`].
pub fn unpack_u64_at(level: Level, width: u32, bytes: &[u8], base: u64, out: &mut [u64]) -> bool {
    match level {
        Level::Baseline => {
            unpack::unpack_u64(width, bytes, base, out);
            true
        }
        #[cfg(target_arch = "x86_64")]
        Level::Avx2 => {
            struct Fast<'a, 'b> {
                simd: pulp::x86::V3,
                width: u32,
                bytes: &'a [u8],
                base: u64,
                out: &'b mut [u64],
            }
            impl pulp::WithSimd for Fast<'_, '_> {
                type Output = ();

                #[inline(always)]
                fn with_simd<S: pulp::Simd>(self, _simd: S) {
                    unpack::unpack_u64_avx2(self.simd, self.width, self.bytes, self.base, self.out);
                }
            }
            let Some(simd) = pulp::x86::V3::try_new() else {
                return false;
            };
            pulp::Simd::vectorize(
                simd,
                Fast {
                    simd,
                    width,
                    bytes,
                    base,
                    out,
                },
            );
            true
        }
        _ => false,
    }
}

/// The instruction set [`translate_codes`] runs at for a dictionary of
/// `entries` values in this process.
#[must_use]
pub fn translate_level(entries: usize) -> Level {
    #[cfg(target_arch = "x86_64")]
    if entries <= codes::AVX2_ENTRIES && avx2().is_some() {
        return Level::Avx2;
    }
    let _ = entries;
    Level::Baseline
}

/// Appends `translation[code]` for each little-endian `u32` code in `raw`
/// (see [`codes`]). `false`, with `out` as it was, when a code is out of
/// bounds for `translation`.
pub fn translate_codes(raw: &[u8], translation: &[u32], out: &mut Vec<u32>) -> bool {
    let level = translate_level(translation.len());
    match translate_codes_at(level, raw, translation, out) {
        Some(in_bounds) => in_bounds,
        // The dispatched level is one the CPU has; the portable lookup
        // answers for any that is not.
        None => codes::translate_codes(raw, translation, out),
    }
}

/// [`translate_codes`] over codes already held as `u32`s.
pub fn translate_u32(codes: &[u32], translation: &[u32], out: &mut Vec<u32>) -> bool {
    // The vector kernel exists on little-endian x86-64 only, where a
    // `u32`'s bytes are the little-endian code the byte form reads.
    #[cfg(target_arch = "x86_64")]
    if translate_level(translation.len()) != Level::Baseline {
        return translate_codes(pulp::bytemuck::cast_slice(codes), translation, out);
    }
    let start = out.len();
    let mut in_bounds = true;
    out.extend(codes.iter().map(|code| {
        translation.get(*code as usize).copied().unwrap_or_else(|| {
            in_bounds = false;
            0
        })
    }));
    if !in_bounds {
        out.truncate(start);
    }
    in_bounds
}

/// [`translate_codes`] at a named level, for tests and measurement: `None`,
/// with `out` untouched, when this CPU lacks the level or the level has no
/// kernel of its own.
pub fn translate_codes_at(
    level: Level,
    raw: &[u8],
    translation: &[u32],
    out: &mut Vec<u32>,
) -> Option<bool> {
    match level {
        Level::Baseline => Some(codes::translate_codes(raw, translation, out)),
        #[cfg(target_arch = "x86_64")]
        Level::Avx2 => {
            struct Fast<'a, 'b, 'c> {
                simd: pulp::x86::V3,
                raw: &'a [u8],
                translation: &'b [u32],
                out: &'c mut Vec<u32>,
            }
            impl pulp::WithSimd for Fast<'_, '_, '_> {
                type Output = bool;

                #[inline(always)]
                fn with_simd<S: pulp::Simd>(self, _simd: S) -> bool {
                    codes::translate_codes_avx2(self.simd, self.raw, self.translation, self.out)
                }
            }
            let simd = pulp::x86::V3::try_new()?;
            Some(pulp::Simd::vectorize(
                simd,
                Fast {
                    simd,
                    raw,
                    translation,
                    out,
                },
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
