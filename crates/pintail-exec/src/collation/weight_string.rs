//! The bytes `WEIGHT_STRING` returns: the weights a collation compares a
//! string by, written out big-endian.
//!
//! These are the weights themselves, not the sort keys the executor groups
//! and joins by. A sort key drops trailing spaces and closes with one; the
//! weights of `'a '` end in the space that was written. A length cuts the
//! string to that many characters first, and a collation that pads (every
//! one here but `utf8mb4_0900_ai_ci`) then fills it up to that many with
//! the weight of a space.

use super::ai_ci_table::{AI_CI_ARENA, AI_CI_DERIVED, AI_CI_RUNS, AI_CI_SEQUENCES};
use super::{Collation, GENERAL_CI_SPACE, UNICODE_CI_SPACE, UnicodeCiWeights, general_ci_weight};

/// Appends the weights one character has under `utf8mb4_0900_ai_ci`: none
/// for an ignorable character, one for most, several for an expansion.
///
/// A tabulated character is in a run of single weights or among those
/// weighing none or several. A Hangul syllable weighs as the jamo it is
/// composed of. Everything else derives two weights from its code point:
/// by the range it falls in, or by the rule for a character that is in
/// none.
fn ai_ci_weights(point: u32, weights: &mut Vec<u16>) {
    const HANGUL_FIRST: u32 = 0xac00;
    const HANGUL_LAST: u32 = 0xd7a3;
    const UNASSIGNED_BASE: u16 = 0xfbc0;
    let low = |value: u32| u16::try_from(value & 0xffff).unwrap_or(0);
    let run = AI_CI_RUNS.partition_point(|(first, _, _, _)| *first <= point);
    if let Some((first, length, weight, step)) = run.checked_sub(1).map(|index| AI_CI_RUNS[index])
        && point - first < u32::from(length)
    {
        weights.push(weight + u16::from(step) * low(point - first));
        return;
    }
    if let Ok(index) = AI_CI_SEQUENCES.binary_search_by_key(&point, |(point, _, _)| *point) {
        let (_, offset, count) = AI_CI_SEQUENCES[index];
        let start = usize::from(offset);
        weights.extend_from_slice(&AI_CI_ARENA[start..start + usize::from(count)]);
        return;
    }
    if (HANGUL_FIRST..=HANGUL_LAST).contains(&point) {
        let index = point - HANGUL_FIRST;
        ai_ci_weights(0x1100 + index / 588, weights);
        ai_ci_weights(0x1161 + index % 588 / 28, weights);
        if !index.is_multiple_of(28) {
            ai_ci_weights(0x11a7 + index % 28, weights);
        }
        return;
    }
    let range = AI_CI_DERIVED.partition_point(|(first, _, _, _)| *first <= point);
    match range.checked_sub(1).map(|index| AI_CI_DERIVED[index]) {
        Some((first, last, base, true)) if point <= last => {
            weights.extend([base, 0x8000 + low(point - first)]);
        }
        Some((_, last, base, false)) if point <= last => {
            weights.extend([base + low(point >> 15), low(point & 0x7fff) | 0x8000]);
        }
        _ => weights.extend([
            UNASSIGNED_BASE + low(point >> 15),
            low(point & 0x7fff) | 0x8000,
        ]),
    }
}

/// The weight bytes of `text` under `collation`, cut and padded to
/// `characters` when a length is given.
///
/// `None` for a collation other than the four below.
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
            let mut weights = Vec::with_capacity(text.len());
            for character in text.chars() {
                ai_ci_weights(u32::from(character), &mut weights);
            }
            wide(&mut bytes, weights.into_iter());
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
    fn the_default_collation_weighs_every_shape_of_character() {
        let default = Collation::Utf8mb40900AiCi;
        assert_eq!(hex(default, "a b", None).as_deref(), Some("1C4702091C60"));
        // A control character weighs nothing, and neither does a mark.
        assert_eq!(
            hex(default, "a\u{0}\u{9}-", None).as_deref(),
            Some("1C470201020D")
        );
        assert_eq!(hex(default, "e\u{301}", None).as_deref(), Some("1CAA"));
        // An accent folds away, and an expansion weighs several.
        assert_eq!(hex(default, "\u{e9}", None).as_deref(), Some("1CAA"));
        assert_eq!(
            hex(default, "\u{df}\u{c6}", None).as_deref(),
            Some("1E711E711C471CAA")
        );
        // An ideograph derives its weights; an emoji is tabulated.
        assert_eq!(
            hex(default, "\u{4e00}\u{1f600}", None).as_deref(),
            Some("FB40CE0015FB")
        );
        // A Hangul syllable weighs as its jamo.
        assert_eq!(
            hex(default, "\u{ac01}", None),
            hex(default, "\u{1100}\u{1161}\u{11a8}", None)
        );
        // A character in no table follows the rule for one that is not.
        assert_eq!(
            hex(default, "\u{10ffff}", None).as_deref(),
            Some("FBE1FFFF")
        );
        assert_eq!(hex(Collation::Utf8mb40900AsCs, "a", None), None);
    }
}
