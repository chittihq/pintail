//! The bytes `WEIGHT_STRING` returns: the weights a collation compares a
//! string by, written out big-endian.
//!
//! These are the weights themselves, not the sort keys the executor groups
//! and joins by. A sort key drops trailing spaces and closes with one; the
//! weights of `'a '` end in the space that was written. A length cuts the
//! string to that many characters first, and a collation that pads (every
//! one here but `utf8mb4_0900_ai_ci`) then fills it up to that many with
//! the weight of a space.

use super::{Collation, GENERAL_CI_SPACE, UNICODE_CI_SPACE, UnicodeCiWeights, general_ci_weight};

/// The primary weight of each ASCII character under `utf8mb4_0900_ai_ci`;
/// zero for a character that weighs nothing. No ASCII character weighs
/// more than one, and none contracts with another.
#[rustfmt::skip]
const AI_CI_ASCII: [u16; 128] = [
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0209, 0x0260, 0x030C, 0x0398, 0x1C12, 0x0399, 0x0396, 0x0305,
    0x0317, 0x0318, 0x038F, 0x0616, 0x0222, 0x020D, 0x0277, 0x0394,
    0x1C3D, 0x1C3E, 0x1C3F, 0x1C40, 0x1C41, 0x1C42, 0x1C43, 0x1C44,
    0x1C45, 0x1C46, 0x0239, 0x0234, 0x061A, 0x061B, 0x061C, 0x0266,
    0x038E, 0x1C47, 0x1C60, 0x1C7A, 0x1C8F, 0x1CAA, 0x1CE5, 0x1CF4,
    0x1D18, 0x1D32, 0x1D4C, 0x1D65, 0x1D77, 0x1DAA, 0x1DB9, 0x1DDD,
    0x1E0C, 0x1E21, 0x1E33, 0x1E71, 0x1E95, 0x1EB5, 0x1EE3, 0x1EF5,
    0x1EFF, 0x1F0B, 0x1F21, 0x0319, 0x0395, 0x031A, 0x0485, 0x020B,
    0x0482, 0x1C47, 0x1C60, 0x1C7A, 0x1C8F, 0x1CAA, 0x1CE5, 0x1CF4,
    0x1D18, 0x1D32, 0x1D4C, 0x1D65, 0x1D77, 0x1DAA, 0x1DB9, 0x1DDD,
    0x1E0C, 0x1E21, 0x1E33, 0x1E71, 0x1E95, 0x1EB5, 0x1EE3, 0x1EF5,
    0x1EFF, 0x1F0B, 0x1F21, 0x031B, 0x061E, 0x031C, 0x0620, 0x0000,
];

/// The weight bytes of `text` under `collation`, cut and padded to
/// `characters` when a length is given.
///
/// `None` where the weights are not known byte for byte: a collation other
/// than the four below, and text beyond ASCII under `utf8mb4_0900_ai_ci`,
/// whose full table is not carried.
#[must_use]
pub fn weight_string(
    collation: Collation,
    text: &str,
    characters: Option<usize>,
) -> Option<Vec<u8>> {
    let kept = characters.map_or(text.len(), |limit| {
        text.char_indices()
            .nth(limit)
            .map_or(text.len(), |(index, _)| index)
    });
    let text = &text[..kept];
    let missing = characters.map_or(0, |limit| limit.saturating_sub(text.chars().count()));
    let mut bytes = Vec::with_capacity(text.len().saturating_add(missing).saturating_mul(3));
    match collation {
        Collation::Utf8mb40900AiCi => {
            if !text.is_ascii() {
                return None;
            }
            wide(
                &mut bytes,
                text.bytes()
                    .map(|byte| AI_CI_ASCII[usize::from(byte)])
                    .filter(|weight| *weight != 0),
            );
        }
        Collation::Utf8mb4GeneralCi => {
            wide(
                &mut bytes,
                text.chars()
                    .map(general_ci_weight)
                    .chain(std::iter::repeat_n(GENERAL_CI_SPACE, missing)),
            );
        }
        Collation::Utf8mb4UnicodeCi => {
            wide(
                &mut bytes,
                UnicodeCiWeights::new(text).chain(std::iter::repeat_n(UNICODE_CI_SPACE, missing)),
            );
        }
        Collation::Utf8mb4Bin => {
            for point in text
                .chars()
                .map(|character| character as u32)
                .chain(std::iter::repeat_n(0x20, missing))
            {
                bytes.extend_from_slice(&point.to_be_bytes()[1..]);
            }
        }
        _ => return None,
    }
    Some(bytes)
}

/// The bytes a result made exactly so wide is filled with: the weight of
/// a space where the collation pads, zero where it does not.
#[must_use]
pub fn weight_string_pad(collation: Collation) -> &'static [u8] {
    match collation {
        Collation::Utf8mb4GeneralCi => &[0x00, 0x20],
        Collation::Utf8mb4UnicodeCi => &[0x02, 0x09],
        Collation::Utf8mb4Bin => &[0x00, 0x00, 0x20],
        _ => &[0],
    }
}

fn wide(bytes: &mut Vec<u8>, weights: impl Iterator<Item = u16>) {
    for weight in weights {
        bytes.extend_from_slice(&weight.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{Collation, weight_string};

    fn hex(collation: Collation, text: &str, characters: Option<usize>) -> Option<String> {
        use std::fmt::Write as _;
        weight_string(collation, text, characters).map(|bytes| {
            bytes.iter().fold(String::new(), |mut hex, byte| {
                write!(hex, "{byte:02X}").expect("write to a string");
                hex
            })
        })
    }

    #[test]
    fn weights_keep_what_a_sort_key_drops() {
        let general = Collation::Utf8mb4GeneralCi;
        assert_eq!(hex(general, "Ab ", None).as_deref(), Some("004100420020"));
        assert_eq!(
            hex(general, "\u{e9}\u{df}\u{4e00}\u{1f600}", None).as_deref(),
            Some("004500534E00FFFD")
        );
        let unicode = Collation::Utf8mb4UnicodeCi;
        assert_eq!(hex(unicode, "Ab ", None).as_deref(), Some("0E330E4A0209"));
        assert_eq!(
            hex(unicode, "\u{e9}\u{df}\u{4e00}\u{1f600}", None).as_deref(),
            Some("0E8B0FEA0FEAFB40CE00FFFD")
        );
        assert_eq!(
            hex(Collation::Utf8mb4Bin, "\u{e9}\u{1f600}", None).as_deref(),
            Some("0000E901F600")
        );
    }

    #[test]
    fn a_length_cuts_by_character_and_pads_where_the_collation_pads() {
        let general = Collation::Utf8mb4GeneralCi;
        assert_eq!(
            hex(general, "ab", Some(4)).as_deref(),
            Some("0041004200200020")
        );
        assert_eq!(
            hex(general, "abcdef", Some(4)).as_deref(),
            Some("0041004200430044")
        );
        let unicode = Collation::Utf8mb4UnicodeCi;
        assert_eq!(
            hex(unicode, "ab", Some(4)).as_deref(),
            Some("0E330E4A02090209")
        );
        // One character is kept whole, however many weights it expands to.
        assert_eq!(hex(unicode, "\u{df}", Some(1)).as_deref(), Some("0FEA0FEA"));
        assert_eq!(
            hex(Collation::Utf8mb4Bin, "ab", Some(4)).as_deref(),
            Some("000061000062000020000020")
        );
        // The default collation does not pad.
        let default = Collation::Utf8mb40900AiCi;
        assert_eq!(hex(default, "ab", Some(4)).as_deref(), Some("1C471C60"));
        assert_eq!(
            hex(default, "abcdef", Some(4)).as_deref(),
            Some("1C471C601C7A1C8F")
        );
    }

    #[test]
    fn the_default_collation_answers_ascii_alone() {
        let default = Collation::Utf8mb40900AiCi;
        assert_eq!(hex(default, "a b", None).as_deref(), Some("1C4702091C60"));
        // A control character weighs nothing.
        assert_eq!(
            hex(default, "a\u{0}\u{9}-", None).as_deref(),
            Some("1C470201020D")
        );
        assert_eq!(hex(default, "\u{e9}", None), None);
        assert_eq!(hex(Collation::Utf8mb40900AsCs, "a", None), None);
    }
}
