//! Fixed-width Thai keys: leading vowels follow their consonant; marks carry
//! the position of preceding consonants. The suffix retains the input width.

pub(super) fn positional_weights(bytes: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(bytes.len());
    let mut input = bytes.iter().copied().peekable();
    let mut position = 248_u8;
    let mut last_mark = None;
    while let Some(byte) = input.next() {
        if matches!(byte, 0xe0..=0xe4)
            && input
                .peek()
                .is_some_and(|next| matches!(*next, 0xa1..=0xce))
        {
            key.push(input.next().expect("consonant follows vowel"));
            key.push(byte);
            continue;
        }
        if byte < 128 || matches!(byte, 0xa1..=0xce) {
            position = position.wrapping_sub(8);
        }
        let mark = match byte {
            0xec => Some(1),
            0xe7 => Some(2),
            0xe8..=0xeb => Some(byte - 0xe5),
            _ => None,
        };
        if let Some(mark) = mark {
            last_mark = Some(position.wrapping_add(mark));
        } else {
            key.push(byte.to_ascii_lowercase());
        }
    }
    if let Some(mark) = last_mark {
        // Secondary marks share a fixed-width suffix. Earlier vacated slots
        // retain the terminal input byte; the final slot carries the last mark.
        key.resize(bytes.len() - 1, *bytes.last().expect("a mark was present"));
        key.push(mark);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::positional_weights;

    #[test]
    fn every_nonzero_byte_pair_matches_the_oracle_weight_digest() {
        let mut digest = 14_695_981_039_346_656_037_u64;
        for first in 1..=255 {
            for second in 1..=255 {
                for weight in positional_weights(&[first, second]) {
                    digest = (digest ^ u64::from(weight)).wrapping_mul(1_099_511_628_211);
                }
            }
        }
        assert_eq!(digest, 3_202_166_020_080_521_673_u64);
    }

    #[test]
    fn positional_marks_keep_width_and_wrap_without_repeated_shifts() {
        for (input, expected) in [
            (vec![0xe0, 0xa1, 0xe8], vec![0xa1, 0xe0, 0xfb]),
            (vec![0xa1, 0xe8, 0xa2, 0xe9], vec![0xa1, 0xa2, 0xe9, 0xec]),
            (vec![0xe8, 0xe9, b'A'], vec![b'a', b'A', 0xfc]),
            (vec![b'a', 0], vec![b'a', 0]),
        ] {
            assert_eq!(positional_weights(&input), expected);
        }
        let mut input = vec![0xa1; 16];
        input.push(0xe8);
        let mut expected = vec![0xa1; 16];
        expected.push(0x7b);
        assert_eq!(positional_weights(&input), expected);
        let input = vec![0xe8; 100_000];
        let key = positional_weights(&input);
        assert_eq!(key.len(), input.len());
        assert_eq!(key.last(), Some(&0xfb));
    }
}
