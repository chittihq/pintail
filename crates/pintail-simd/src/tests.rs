use super::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const LENGTHS: [usize; 14] = [0, 1, 7, 8, 9, 63, 64, 65, 127, 128, 129, 1000, 1024, 4099];

fn reference_mask<T: Copy>(values: &[T], predicate: impl Fn(T) -> bool) -> Vec<u64> {
    let mut words = vec![0_u64; values.len().div_ceil(64)];
    for (row, &value) in values.iter().enumerate() {
        if predicate(value) {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

fn masked<T: Copy>(values: &[T], kernel: impl FnOnce(&[T], &mut [u64])) -> Vec<u64> {
    // A poisoned buffer one word longer than needed: the kernel must write
    // every needed word and leave the spare one alone.
    let mut words = vec![0xA5A5_A5A5_A5A5_A5A5_u64; values.len().div_ceil(64) + 1];
    kernel(values, &mut words);
    assert_eq!(words.pop(), Some(0xA5A5_A5A5_A5A5_A5A5));
    words
}

#[test]
fn sums_are_exact_at_the_i64_extremes() {
    for &len in &LENGTHS {
        let values = vec![i64::MAX; len];
        assert_eq!(sum_i64(&values), i128::from(i64::MAX) * len as i128);
        let values = vec![i64::MIN; len];
        assert_eq!(sum_i64(&values), i128::from(i64::MIN) * len as i128);
        assert_eq!(portable::sum_i64(&values), sum_i64(&values));
    }
}

#[test]
#[allow(clippy::float_cmp)]
fn reductions_match_scalar_folds() {
    let mut rng = StdRng::seed_from_u64(7);
    for &len in &LENGTHS {
        let ints: Vec<i64> = (0..len).map(|_| rng.random()).collect();
        assert_eq!(
            sum_i64(&ints),
            ints.iter().map(|&v| i128::from(v)).sum::<i128>()
        );
        assert_eq!(min_i64(&ints), ints.iter().copied().min());
        assert_eq!(max_i64(&ints), ints.iter().copied().max());

        // Integer-valued floats sum exactly in any order.
        let floats: Vec<f64> = (0..len)
            .map(|_| f64::from(rng.random_range(-1000..1000)))
            .collect();
        assert_eq!(sum_f64(&floats), floats.iter().sum::<f64>());
        assert_eq!(min_f64(&floats), floats.iter().copied().reduce(f64::min));
        assert_eq!(max_f64(&floats), floats.iter().copied().reduce(f64::max));
    }
}

#[test]
fn comparisons_match_per_row_evaluation() {
    let mut rng = StdRng::seed_from_u64(11);
    let ops = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ];
    for &len in &LENGTHS {
        let wide: Vec<i64> = (0..len).map(|_| rng.random_range(-20..20)).collect();
        let narrow: Vec<i32> = (0..len).map(|_| rng.random_range(-20..20)).collect();
        let codes: Vec<u32> = (0..len).map(|_| rng.random_range(0..40)).collect();
        for op in ops {
            let test = move |ordering: std::cmp::Ordering| match op {
                CmpOp::Eq => ordering.is_eq(),
                CmpOp::Ne => ordering.is_ne(),
                CmpOp::Lt => ordering.is_lt(),
                CmpOp::Le => ordering.is_le(),
                CmpOp::Gt => ordering.is_gt(),
                CmpOp::Ge => ordering.is_ge(),
            };
            assert_eq!(
                masked(&wide, |v, out| compare_i64(v, op, 3, out)),
                reference_mask(&wide, |v| test(v.cmp(&3)))
            );
            assert_eq!(
                masked(&narrow, |v, out| compare_i32(v, op, -4, out)),
                reference_mask(&narrow, |v| test(v.cmp(&-4)))
            );
            assert_eq!(
                masked(&codes, |v, out| compare_u32(v, op, 17, out)),
                reference_mask(&codes, |v| test(v.cmp(&17)))
            );
        }
        for (low, high) in [(-5, 5), (3, 3), (5, -5), (i64::MIN, 0), (0, i64::MAX)] {
            assert_eq!(
                masked(&wide, |v, out| between_i64(v, low, high, out)),
                reference_mask(&wide, |v| (low..=high).contains(&v))
            );
        }
        for (low, high) in [(-5, 5), (i32::MIN, i32::MAX), (7, 2)] {
            assert_eq!(
                masked(&narrow, |v, out| between_i32(v, low, high, out)),
                reference_mask(&narrow, |v| (low..=high).contains(&v))
            );
        }
        for (low, high) in [(0, 0), (10, 30), (0, u32::MAX)] {
            assert_eq!(
                masked(&codes, |v, out| between_u32(v, low, high, out)),
                reference_mask(&codes, |v| (low..=high).contains(&v))
            );
        }
    }
}

#[test]
fn between_handles_the_full_signed_range() {
    let values = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
    let mut out = [0_u64; 1];
    between_i64(&values, i64::MIN, i64::MAX, &mut out);
    assert_eq!(out[0], 0b111_1111);
    between_i64(&values, -1, 1, &mut out);
    assert_eq!(out[0], 0b001_1100);
    between_i64(&values, i64::MAX, i64::MAX, &mut out);
    assert_eq!(out[0], 0b100_0000);
}

#[test]
fn mask_expansion_matches_bit_walk() {
    let mut rng = StdRng::seed_from_u64(13);
    for &len in &LENGTHS {
        for density in [0.0, 0.05, 0.3, 0.9, 1.0] {
            let mut words: Vec<u64> = (0..len.div_ceil(64))
                .map(|_| {
                    (0..64).fold(0, |word, bit| {
                        word | (u64::from(rng.random_bool(density)) << bit)
                    })
                })
                .collect();
            // Stray bits past `len` must be ignored.
            if let Some(last) = words.last_mut() {
                *last |= u64::MAX << (len % 64) & if len % 64 == 0 { 0 } else { u64::MAX };
            }
            let expected: Vec<u32> = (0..len)
                .filter(|&row| words[row / 64] >> (row % 64) & 1 == 1)
                .map(|row| u32::try_from(row).unwrap())
                .collect();
            let mut out = vec![99];
            mask_to_indices(&words, len, &mut out);
            assert_eq!(out[0], 99, "existing output is kept");
            assert_eq!(
                &out[1..],
                expected.as_slice(),
                "len {len} density {density}"
            );
        }
    }
}

#[test]
fn gather_decodes_and_compacts() {
    let mut rng = StdRng::seed_from_u64(17);
    let dictionary: Vec<i64> = (0..300).map(|_| rng.random()).collect();
    for &len in &LENGTHS {
        let codes: Vec<u32> = (0..len).map(|_| rng.random_range(0..300)).collect();
        let mut out = Vec::new();
        gather(&dictionary, &codes, &mut out);
        let expected: Vec<i64> = codes
            .iter()
            .map(|&code| dictionary[code as usize])
            .collect();
        assert_eq!(out, expected);
    }
}

#[test]
#[should_panic(expected = "out of bounds")]
fn gather_rejects_an_out_of_range_code() {
    let mut out = Vec::new();
    gather(&[1_u8, 2, 3], &[0, 3], &mut out);
}

#[test]
fn level_is_reported() {
    assert!(!level().name().is_empty());
}

#[test]
#[allow(clippy::float_cmp)]
fn grouped_aggregates_match_per_row_updates() {
    let mut rng = StdRng::seed_from_u64(19);
    for &len in &LENGTHS {
        for group_count in [1_u32, 8, 64, 65, 1000] {
            let groups: Vec<u32> = (0..len).map(|_| rng.random_range(0..group_count)).collect();
            let values: Vec<i64> = (0..len).map(|_| rng.random()).collect();
            let wide: Vec<i128> = values.iter().map(|&v| i128::from(v) << 40).collect();
            let floats: Vec<f64> = values
                .iter()
                .map(|&v| f64::from(i32::try_from(v >> 40).unwrap()) / 7.0)
                .collect();
            let slots = group_count as usize;

            let (mut sums, mut wide_sums, mut float_sums) =
                (vec![0_i128; slots], vec![0_i128; slots], vec![0.0; slots]);
            let (mut counts, mut mins, mut maxs) = (
                vec![0_u64; slots],
                vec![i64::MAX; slots],
                vec![i64::MIN; slots],
            );
            let (mut e_sums, mut e_wide, mut e_floats) =
                (sums.clone(), wide_sums.clone(), float_sums.clone());
            let (mut e_counts, mut e_mins, mut e_maxs) =
                (counts.clone(), mins.clone(), maxs.clone());
            for (row, &group) in groups.iter().enumerate() {
                let g = group as usize;
                e_sums[g] += i128::from(values[row]);
                e_wide[g] = e_wide[g].wrapping_add(wide[row]);
                e_floats[g] += floats[row];
                e_counts[g] += 1;
                e_mins[g] = e_mins[g].min(values[row]);
                e_maxs[g] = e_maxs[g].max(values[row]);
            }
            sum_by_group_i64(&values, &groups, &mut sums);
            sum_by_group_i128(&wide, &groups, &mut wide_sums);
            sum_by_group_f64(&floats, &groups, &mut float_sums);
            count_by_group(&groups, &mut counts);
            let (mut fused_sums, mut fused_counts) = (vec![0_i128; slots], vec![0_u64; slots]);
            sum_count_by_group_i64(&values, &groups, &mut fused_sums, &mut fused_counts);
            assert_eq!(fused_sums, e_sums);
            assert_eq!(fused_counts, e_counts);
            min_by_group_i64(&values, &groups, &mut mins);
            max_by_group_i64(&values, &groups, &mut maxs);
            assert_eq!(sums, e_sums);
            assert_eq!(wide_sums, e_wide);
            assert_eq!(float_sums, e_floats, "float sums keep row order");
            assert_eq!(counts, e_counts);
            assert_eq!(mins, e_mins);
            assert_eq!(maxs, e_maxs);
        }
    }
}

#[test]
#[should_panic(expected = "out of bounds")]
fn grouped_rejects_an_out_of_range_group() {
    let groups = vec![0, 1, 2, 3];
    let mut counts = vec![0_u64; 3];
    count_by_group(&groups, &mut counts);
}

#[test]
fn decimal_sum_reports_overflow() {
    assert_eq!(sum_i128(&[1, 2, 3, 4, 5]), Some(15));
    assert_eq!(sum_i128(&[i128::MAX, 1]), None);
    assert_eq!(sum_i128(&[i128::MAX, 0, 0, 0, 1]), None);
    assert_eq!(sum_i128(&[i128::MAX, -1, 0, 0, 1]), Some(i128::MAX));
}

#[test]
fn bools_pack_into_words() {
    let mut rng = StdRng::seed_from_u64(23);
    for &len in &LENGTHS {
        let bools: Vec<bool> = (0..len).map(|_| rng.random_bool(0.6)).collect();
        assert_eq!(
            masked(&bools, pack_bools),
            reference_mask(&bools, |flag| flag)
        );
    }
}

/// The decode spelled bit by bit, independent of every kernel.
fn unpack_reference(width: u32, bytes: &[u8], count: usize) -> Vec<u64> {
    (0..count)
        .map(|index| {
            (0..width as usize).fold(0_u64, |value, bit| {
                let position = index * width as usize + bit;
                let set = bytes[position / 8] >> (position % 8) & 1;
                value | u64::from(set) << bit
            })
        })
        .collect()
}

/// Every level this CPU has, the dispatched one included.
fn unpack_levels() -> Vec<Level> {
    [Level::Baseline, Level::Avx2]
        .into_iter()
        .filter(|level| unpack_u64_at(*level, 1, &[0], 0, &mut [0]))
        .collect()
}

fn assert_unpacks(width: u32, bytes: &[u8], count: usize) {
    const POISON: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    let plain = unpack_reference(width, bytes, count);
    // No base; one that carries out of the value's bits; one that wraps.
    for base in [0_u64, 0x0123_4567_89AB_CDEF, u64::MAX] {
        let expected: Vec<u64> = plain.iter().map(|value| value.wrapping_add(base)).collect();
        for level in unpack_levels() {
            // A poisoned slot either side: the kernel writes its values only.
            let mut out = vec![POISON; count + 2];
            assert!(unpack_u64_at(
                level,
                width,
                bytes,
                base,
                &mut out[1..=count]
            ));
            let context = format!("{level:?} width {width} count {count} base {base:#x}");
            assert_eq!(out[0], POISON, "{context}");
            assert_eq!(out[count + 1], POISON, "{context}");
            assert_eq!(&out[1..=count], expected.as_slice(), "{context}");
        }
        let mut out = vec![0; count];
        unpack_u64(width, bytes, base, &mut out);
        assert_eq!(out, expected, "dispatched width {width} count {count}");
        let mut signed = vec![0_i64; count];
        unpack_i64(width, bytes, base.cast_signed(), &mut signed);
        let signed: Vec<u64> = signed.into_iter().map(i64::cast_unsigned).collect();
        assert_eq!(signed, expected, "signed width {width} count {count}");
    }
}

#[test]
fn unpack_matches_the_bitwise_decode_at_every_width_and_length() {
    let mut rng = StdRng::seed_from_u64(56);
    for width in 0..=64_u32 {
        for count in (0..=300).chain([511, 512, 513, 1024, 4099, 16_384]) {
            let exact = (count * width as usize).div_ceil(8);
            let mut bytes: Vec<u8> = (0..exact).map(|_| rng.random()).collect();
            assert_unpacks(width, &bytes, count);
            // The payload may continue past the values: later bytes are
            // another column's, and must not leak into the last values.
            bytes.extend((0..count % 97).map(|_| rng.random::<u8>()));
            assert_unpacks(width, &bytes, count);
        }
    }
}

#[test]
fn unpack_decodes_from_every_byte_offset_of_a_payload() {
    let mut rng = StdRng::seed_from_u64(57);
    for width in 1..=64_u32 {
        // Groups of eight values start on bytes: decode from each such
        // start, at every distance from the payload's end.
        let total = 96_usize;
        let bytes: Vec<u8> = (0..total * width as usize / 8)
            .map(|_| rng.random())
            .collect();
        let expected = unpack_reference(width, &bytes, total);
        for run in 0..total / 8 {
            let start = run * width as usize;
            for count in 0..=total - run * 8 {
                for level in unpack_levels() {
                    let mut out = vec![0; count];
                    assert!(unpack_u64_at(level, width, &bytes[start..], 0, &mut out));
                    assert_eq!(
                        out,
                        &expected[run * 8..run * 8 + count],
                        "{level:?} width {width} run {run} count {count}"
                    );
                }
            }
        }
    }
}

#[test]
fn unpack_holds_the_extreme_patterns() {
    for width in 1..=64_u32 {
        for fill in [0x00_u8, 0xFF, 0xAA, 0x55, 0x80, 0x01] {
            let bytes = vec![fill; 301 * width as usize / 8 + 1];
            assert_unpacks(width, &bytes, 301);
        }
    }
}

#[test]
#[should_panic(expected = "need")]
fn unpack_refuses_a_short_payload() {
    unpack_u64(21, &[0; 20], 0, &mut [0; 8]);
}

#[test]
fn this_machine_reports_the_levels_it_exercised() {
    // Printed with `--nocapture`: which kernels the equivalence tests ran.
    eprintln!(
        "unpack levels exercised: {:?}; dispatched: 21 bits at {:?}, 57 at {:?}, 64 at {:?}",
        unpack_levels(),
        unpack_level(21),
        unpack_level(57),
        unpack_level(64)
    );
    assert!(unpack_levels().contains(&Level::Baseline));
}

#[test]
fn code_translation_matches_the_checked_lookup_at_every_level() {
    let mut rng = StdRng::seed_from_u64(58);
    let levels: Vec<Level> = [Level::Baseline, Level::Avx2]
        .into_iter()
        .filter(|level| translate_codes_at(*level, &[], &[], &mut Vec::new()).is_some())
        .collect();
    for entries in (0..=40_usize).chain([255, 256, 1024]) {
        let translation: Vec<u32> = (0..entries).map(|_| rng.random()).collect();
        for rows in (0..=300_usize).chain([1000, 4099]) {
            // In bounds; then one code just past the table at a random row,
            // and one with only high bits set, which a lookup that masked
            // the index would take for a valid code.
            let bound = u32::try_from(entries).expect("entries fit u32");
            for stray in [None, Some(bound), Some(0x8000_0000), Some(u32::MAX)] {
                if entries == 0 && rows > 0 && stray.is_none() {
                    continue;
                }
                let mut codes: Vec<u32> = (0..rows)
                    .map(|_| rng.random_range(0..bound.max(1)))
                    .collect();
                if let Some(stray) = stray {
                    if rows == 0 {
                        continue;
                    }
                    codes[rng.random_range(0..rows)] = stray;
                }
                let mut raw: Vec<u8> = codes.iter().flat_map(|code| code.to_le_bytes()).collect();
                // Bytes short of a row are not a row.
                raw.extend(std::iter::repeat_n(0xEE, rows % 4));
                let expected: Option<Vec<u32>> = codes
                    .iter()
                    .map(|code| translation.get(*code as usize).copied())
                    .collect();
                for &level in &levels {
                    let mut out = vec![7_u32, 8, 9];
                    let ok = translate_codes_at(level, &raw, &translation, &mut out)
                        .expect("level present");
                    let context = format!("{level:?} entries {entries} rows {rows} {stray:?}");
                    assert_eq!(ok, expected.is_some(), "{context}");
                    assert_eq!(&out[..3], &[7, 8, 9], "{context}");
                    match &expected {
                        Some(values) => assert_eq!(&out[3..], values.as_slice(), "{context}"),
                        None => assert_eq!(out.len(), 3, "{context}"),
                    }
                }
                let mut out = Vec::new();
                assert_eq!(
                    translate_codes(&raw, &translation, &mut out),
                    expected.is_some()
                );
                assert_eq!(&out, expected.as_deref().unwrap_or_default());
                let mut out = vec![5_u32];
                assert_eq!(
                    translate_u32(&codes, &translation, &mut out),
                    expected.is_some()
                );
                assert_eq!(&out[1..], expected.as_deref().unwrap_or_default());
            }
        }
    }
}
