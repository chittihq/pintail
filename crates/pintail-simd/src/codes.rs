//! Dictionary-code translation: each row's block-local code replaced by the
//! code its value has in the chunk's dictionary.
//!
//! The portable form is a bounds-checked table lookup a row. A dictionary
//! of at most eight values - a status, a region - fits an AVX2 register,
//! and a register permutation is then the whole lookup, eight rows an
//! instruction. The bounds check becomes one running maximum compared once
//! at the end.

/// Largest dictionary the AVX2 kernel translates.
pub const AVX2_ENTRIES: usize = 8;

/// Appends `translation[code]` for each little-endian `u32` code in `raw`
/// (`raw.len() / 4` rows). `false`, with `out` as it was, when a code is
/// out of bounds for `translation`.
#[inline(always)]
pub fn translate_codes(raw: &[u8], translation: &[u32], out: &mut Vec<u32>) -> bool {
    let start = out.len();
    out.reserve(raw.len() / 4);
    translate_tail(raw, translation, start, out)
}

/// The portable lookup of `raw`'s rows, undoing everything past `start`
/// on a code out of bounds.
#[inline(always)]
fn translate_tail(raw: &[u8], translation: &[u32], start: usize, out: &mut Vec<u32>) -> bool {
    let mut in_bounds = true;
    out.extend(raw.chunks_exact(4).map(|chunk| {
        let code = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        translation.get(code as usize).copied().unwrap_or_else(|| {
            in_bounds = false;
            0
        })
    }));
    if !in_bounds {
        out.truncate(start);
    }
    in_bounds
}

/// [`translate_codes`] on AVX2, for dictionaries of at most
/// [`AVX2_ENTRIES`] values; larger ones take the portable lookup.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn translate_codes_avx2(
    simd: pulp::x86::V3,
    raw: &[u8],
    translation: &[u32],
    out: &mut Vec<u32>,
) -> bool {
    use core::arch::x86_64::__m256i;
    const LANES: usize = 8;
    let start = out.len();
    if translation.len() > AVX2_ENTRIES {
        return translate_codes(raw, translation, out);
    }
    let rows = raw.len() / 4;
    let whole = rows / LANES * LANES;
    let mut table = [0_u32; AVX2_ENTRIES];
    table[..translation.len()].copy_from_slice(translation);
    let table: __m256i = pulp::cast(table);
    let mut greatest = simd.avx._mm256_setzero_si256();
    out.resize(start + whole, 0);
    for (codes, translated) in raw
        .chunks_exact(LANES * 4)
        .zip(out[start..].chunks_exact_mut(LANES))
    {
        let codes: __m256i = pulp::cast(<[u8; LANES * 4]>::try_from(codes).expect("codes"));
        greatest = simd.avx2._mm256_max_epu32(greatest, codes);
        let values: [u32; LANES] = pulp::cast(simd.avx2._mm256_permutevar8x32_epi32(table, codes));
        translated.copy_from_slice(&values);
    }
    let greatest: [u32; LANES] = pulp::cast(greatest);
    if whole > 0 && greatest.into_iter().max().unwrap_or(0) as usize >= translation.len() {
        out.truncate(start);
        return false;
    }
    translate_tail(&raw[whole * 4..], translation, start, out)
}
