//! Text collations the executor can compare.
//!
//! One collation is chosen per expression at bind time and carried into the
//! plan, so the row loop dispatches on a value it already holds rather than
//! looking anything up. Adding a collation therefore costs existing queries
//! nothing: `utf8mb4_0900_ai_ci` still reaches the same ICU path it always
//! did.
//!
//! # Why `general_ci` is here at all
//!
//! It is the older, non-Unicode-conformant collation - `MySQL` 5.x's default -
//! and most existing schemas still carry it, because a table keeps whatever
//! collation it was created with. Supporting only `MySQL` 8's modern default
//! meant a source could snapshot, replicate and read back while every `WHERE`,
//! `JOIN`, `GROUP BY` and `ORDER BY` on its text columns was refused.

mod general_ci_table;
mod thai;
mod unicode_ci_table;

use general_ci_table::GENERAL_CI_EXCEPTIONS;
use unicode_ci_table::{
    UNICODE_CI_ARENA, UNICODE_CI_IMPLICIT, UNICODE_CI_SEQUENCES, UNICODE_CI_SINGLES,
};

/// A text collation the executor can compare.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub enum Collation {
    /// Western European flat weights with PAD SPACE semantics.
    Latin1SwedishCi,
    /// Single-byte code values with PAD SPACE semantics.
    Latin1Bin,
    /// Central European single-byte case-insensitive weights.
    Latin2GeneralCi,
    /// Central European encoded byte order.
    Latin2Bin,
    /// Thai encoded positional weights.
    Tis620ThaiCi,
    /// Thai encoded byte order.
    Tis620Bin,
    /// Cyrillic single-byte case-insensitive weights.
    Koi8RGeneralCi,
    /// Cyrillic encoded byte order.
    Koi8RBin,
    /// `MySQL` 8's default: UCA 9.0.0, accent- and case-insensitive.
    #[default]
    Utf8mb40900AiCi,
    /// Accent- and case-sensitive Unicode comparison with NO PAD semantics.
    Utf8mb40900AsCs,
    /// `MySQL` 5.x's default: a flat per-character weight, no expansions or
    /// contractions.
    Utf8mb4GeneralCi,
    /// UCA 4.0.0, accent- and case-insensitive: the collation a schema
    /// created before `MySQL` 8 asked for by name rather than by default.
    /// Unlike `general_ci` a character weighs a sequence here - an ignorable
    /// mark weighs nothing and an expansion weighs several.
    Utf8mb4UnicodeCi,
    /// Byte-wise comparison with PAD SPACE semantics: code points compare by
    /// value and trailing spaces are insignificant. What `MySQL` gives the
    /// results of `JSON_UNQUOTE` and friends - measured live: grouping and
    /// comparing them is case-SENSITIVE even in an `ai_ci` session.
    Utf8mb4Bin,
    /// Not a text collation: `MySQL`'s JSON comparison ladder, carried in
    /// the same per-key slot so JSON documents compare, group and dedupe
    /// structurally wherever a collation already dispatches. Named "json"
    /// internally; no `MySQL` collation name resolves to it by accident
    /// because the binder validates user-written `COLLATE` names first.
    Json,
}

impl Collation {
    /// Resolves a `MySQL` collation name.
    ///
    /// Returns `None` for anything unsupported, so the caller reports which
    /// name it was rather than silently substituting a collation that orders
    /// differently - a wrong answer is worse than a refusal here.
    #[must_use]
    pub fn from_mysql_name(name: &str) -> Option<Self> {
        match pintail_sql::comparison_collation(name).unwrap_or(name) {
            "latin1_swedish_ci" => Some(Self::Latin1SwedishCi),
            "latin1_bin" => Some(Self::Latin1Bin),
            "tis620_thai_ci" => Some(Self::Tis620ThaiCi),
            "tis620_bin" => Some(Self::Tis620Bin),
            "latin2_general_ci" => Some(Self::Latin2GeneralCi),
            "latin2_bin" => Some(Self::Latin2Bin),
            "koi8r_general_ci" => Some(Self::Koi8RGeneralCi),
            "koi8r_bin" => Some(Self::Koi8RBin),
            "utf8mb4_0900_ai_ci" => Some(Self::Utf8mb40900AiCi),
            "utf8mb4_0900_as_cs" => Some(Self::Utf8mb40900AsCs),
            "utf8mb4_general_ci" => Some(Self::Utf8mb4GeneralCi),
            "utf8mb4_unicode_ci" => Some(Self::Utf8mb4UnicodeCi),
            "utf8mb4_bin" => Some(Self::Utf8mb4Bin),
            name if name.eq_ignore_ascii_case("json") => Some(Self::Json),
            _ => None,
        }
    }

    #[must_use]
    pub const fn mysql_name(self) -> &'static str {
        match self {
            Self::Latin1SwedishCi => "latin1_swedish_ci",
            Self::Latin1Bin => "latin1_bin",
            Self::Tis620ThaiCi => "tis620_thai_ci",
            Self::Tis620Bin => "tis620_bin",
            Self::Latin2GeneralCi => "latin2_general_ci",
            Self::Latin2Bin => "latin2_bin",
            Self::Koi8RGeneralCi => "koi8r_general_ci",
            Self::Koi8RBin => "koi8r_bin",
            Self::Utf8mb40900AiCi => "utf8mb4_0900_ai_ci",
            Self::Utf8mb40900AsCs => "utf8mb4_0900_as_cs",
            Self::Utf8mb4GeneralCi => "utf8mb4_general_ci",
            Self::Utf8mb4UnicodeCi => "utf8mb4_unicode_ci",
            Self::Utf8mb4Bin => "utf8mb4_bin",
            Self::Json => "json",
        }
    }
}

fn latin1_weights(text: &str, binary: bool) -> impl Iterator<Item = u16> {
    const ACCENT_WEIGHTS: [u8; 64] = [
        0x41, 0x41, 0x41, 0x41, 0x5c, 0x5b, 0x5c, 0x43, 0x45, 0x45, 0x45, 0x45, 0x49, 0x49, 0x49,
        0x49, 0x44, 0x4e, 0x4f, 0x4f, 0x4f, 0x4f, 0x5d, 0xd7, 0xd8, 0x55, 0x55, 0x55, 0x59, 0x59,
        0xde, 0xdf, 0x41, 0x41, 0x41, 0x41, 0x5c, 0x5b, 0x5c, 0x43, 0x45, 0x45, 0x45, 0x45, 0x49,
        0x49, 0x49, 0x49, 0x44, 0x4e, 0x4f, 0x4f, 0x4f, 0x4f, 0x5d, 0xf7, 0xd8, 0x55, 0x55, 0x55,
        0x59, 0x59, 0xde, 0xff,
    ];
    pintail_types::CharacterSet::Latin1
        .encode(text)
        .into_iter()
        .map(move |byte| {
            u16::from(if binary {
                byte
            } else {
                match byte {
                    b'a'..=b'z' => byte.to_ascii_uppercase(),
                    0xc0..=0xff => ACCENT_WEIGHTS[usize::from(byte - 0xc0)],
                    _ => byte,
                }
            })
        })
}

/// Compares encoded single-byte values, including insignificant trailing spaces.
#[must_use]
pub fn compare_latin1(left: &str, right: &str, binary: bool) -> std::cmp::Ordering {
    compare_padded(
        latin1_weights(left, binary),
        latin1_weights(right, binary),
        u16::from(b' '),
    )
}

/// A single-byte collation key with the same padding rules as comparison.
#[must_use]
pub fn latin1_sort_key(text: &str, binary: bool) -> Vec<u8> {
    padded_sort_key(latin1_weights(text, binary), u16::from(b' '))
}

/// The `general_ci` weight of one character.
///
/// Every character above the BMP weighs `0xFFFD`, so all of them compare
/// equal to each other - every emoji equals every other emoji, and equals a
/// supplementary CJK ideograph. That is real `MySQL` behaviour, verified
/// against a live server, and implementing something more sensible here would
/// be a parity bug rather than an improvement.
fn general_ci_weight(character: char) -> u16 {
    let code_point = character as u32;
    let Ok(bmp) = u16::try_from(code_point) else {
        return 0xfffd;
    };
    match GENERAL_CI_EXCEPTIONS.binary_search_by_key(&bmp, |(point, _)| *point) {
        Ok(index) => GENERAL_CI_EXCEPTIONS[index].1,
        // The table stores only deviations; everything else weighs itself.
        Err(_) => bmp,
    }
}

/// The weight of a space under `general_ci`, which weighs its own code point.
const GENERAL_CI_SPACE: u16 = 0x0020;

/// The weight of a space under `unicode_ci`. A no-break space weighs this
/// too, so a trailing one is as insignificant as a trailing space - which is
/// what `MySQL` answers, and what falls out of comparing weights rather than
/// characters.
const UNICODE_CI_SPACE: u16 = 0x0209;

/// Compares two weight streams under PAD SPACE.
///
/// The shorter operand is padded with spaces, so an exhausted stream keeps
/// yielding the space weight rather than ending the comparison. Trimming
/// trailing spaces instead is nearly the same and quietly wrong: `MySQL`
/// answers that `'a'` is GREATER than `'a\t'`, because the pad puts a space
/// against the tab and a space outweighs it, where trimming makes `'a'` a
/// prefix and therefore smaller.
fn compare_padded<Weight: Ord + Copy>(
    mut left: impl Iterator<Item = Weight>,
    mut right: impl Iterator<Item = Weight>,
    space: Weight,
) -> std::cmp::Ordering {
    loop {
        let (left_weight, right_weight) = (left.next(), right.next());
        if left_weight.is_none() && right_weight.is_none() {
            return std::cmp::Ordering::Equal;
        }
        let ordering = left_weight
            .unwrap_or(space)
            .cmp(&right_weight.unwrap_or(space));
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
}

/// The sort key for a weight stream under PAD SPACE, for hashing, grouping,
/// `DISTINCT` and set membership.
///
/// Trailing space weights carry no information, so they come off; one is then
/// put back as a terminator. That last weight is what makes the key order the
/// same way [`compare_padded`] does: without it a shorter string is a prefix
/// and sorts first, and with it the comparison that decides is the same one
/// the pad would have made. Bytes rather than the hex text the ICU path
/// produces: a key is compared and hashed, never read.
fn padded_sort_key(weights: impl Iterator<Item = u16>, space: u16) -> Vec<u8> {
    let mut key = weights.collect::<Vec<_>>();
    while key.last() == Some(&space) {
        key.pop();
    }
    key.push(space);
    let mut bytes = Vec::with_capacity(key.len() * 2);
    for weight in key {
        bytes.extend_from_slice(&weight.to_be_bytes());
    }
    bytes
}

/// Compares two strings under `general_ci`.
///
/// One character always yields exactly one weight in this collation - there
/// are no expansions or contractions - so the comparison needs no buffering.
#[must_use]
pub fn compare_general_ci(left: &str, right: &str) -> std::cmp::Ordering {
    compare_padded(
        left.chars().map(general_ci_weight),
        right.chars().map(general_ci_weight),
        GENERAL_CI_SPACE,
    )
}

/// Compares two strings under `utf8mb4_bin`: code points by value, trailing
/// spaces insignificant (PAD SPACE, like the two `ci` collations above).
#[must_use]
pub fn compare_bin(left: &str, right: &str) -> std::cmp::Ordering {
    compare_padded(
        left.chars().map(|character| character as u32),
        right.chars().map(|character| character as u32),
        u32::from(b' '),
    )
}

/// The `utf8mb4_bin` sort key: the code points themselves, trailing spaces
/// removed and one put back, for the reason [`padded_sort_key`] gives.
#[must_use]
pub fn bin_sort_key(text: &str) -> Vec<u8> {
    let mut key = text
        .chars()
        .map(|character| character as u32)
        .collect::<Vec<_>>();
    while key.last() == Some(&u32::from(b' ')) {
        key.pop();
    }
    key.push(u32::from(b' '));
    let mut bytes = Vec::with_capacity(key.len() * 4);
    for point in key {
        bytes.extend_from_slice(&point.to_be_bytes());
    }
    bytes
}

/// The `general_ci` sort key.
#[must_use]
pub fn general_ci_sort_key(text: &str) -> Vec<u8> {
    padded_sort_key(text.chars().map(general_ci_weight), GENERAL_CI_SPACE)
}

/// The weights one character contributes under `unicode_ci`: none for an
/// ignorable mark, one for most characters, several for an expansion.
enum Pending {
    Empty,
    One(u16),
    Two(u16, u16),
    Several(&'static [u16]),
}

/// The `unicode_ci` weights of a string, character by character.
///
/// A character is looked up in the tabulated weights and, failing that,
/// derived from its own code point by UCA's rule for a character the table
/// does not name. Everything above the BMP weighs `0xFFFD`, so every one of
/// them compares equal to every other - real `MySQL` behaviour, the same wart
/// `general_ci` has, reproduced deliberately.
struct UnicodeCiWeights<'a> {
    characters: std::str::Chars<'a>,
    pending: Pending,
}

impl<'a> UnicodeCiWeights<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            characters: text.chars(),
            pending: Pending::Empty,
        }
    }
}

impl Iterator for UnicodeCiWeights<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        loop {
            match std::mem::replace(&mut self.pending, Pending::Empty) {
                Pending::One(weight) => return Some(weight),
                Pending::Two(first, second) => {
                    self.pending = Pending::One(second);
                    return Some(first);
                }
                Pending::Several(weights) => {
                    if let Some((first, rest)) = weights.split_first() {
                        self.pending = Pending::Several(rest);
                        return Some(*first);
                    }
                }
                Pending::Empty => {}
            }
            self.pending = unicode_ci_pending(self.characters.next()?);
        }
    }
}

fn unicode_ci_pending(character: char) -> Pending {
    let Ok(point) = u16::try_from(character as u32) else {
        return Pending::One(0xfffd);
    };
    if let Ok(index) = UNICODE_CI_SINGLES.binary_search_by_key(&point, |(point, _)| *point) {
        return Pending::One(UNICODE_CI_SINGLES[index].1);
    }
    if let Ok(index) = UNICODE_CI_SEQUENCES.binary_search_by_key(&point, |(point, _, _)| *point) {
        let (_, offset, count) = UNICODE_CI_SEQUENCES[index];
        let start = offset as usize;
        return Pending::Several(&UNICODE_CI_ARENA[start..start + count as usize]);
    }
    let base = UNICODE_CI_IMPLICIT
        .binary_search_by(|(first, last, _)| {
            if point < *first {
                std::cmp::Ordering::Greater
            } else if point > *last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .map_or(UNASSIGNED_BASE, |index| UNICODE_CI_IMPLICIT[index].2);
    Pending::Two(base + (point >> 15), (point & 0x7fff) | 0x8000)
}

/// The implicit base for a code point in no range: the one UCA gives an
/// unassigned character. Only surrogates reach it, and a `char` is never one.
const UNASSIGNED_BASE: u16 = 0xfbc0;

/// Compares two strings under `unicode_ci`.
#[must_use]
pub fn compare_unicode_ci(left: &str, right: &str) -> std::cmp::Ordering {
    compare_padded(
        UnicodeCiWeights::new(left),
        UnicodeCiWeights::new(right),
        UNICODE_CI_SPACE,
    )
}

/// The `unicode_ci` sort key.
#[must_use]
pub fn unicode_ci_sort_key(text: &str) -> Vec<u8> {
    padded_sort_key(UnicodeCiWeights::new(text), UNICODE_CI_SPACE)
}

/// The key `MySQL`'s `GROUP BY` files a `unicode_ci` value under.
///
/// Comparison pads by weight, and a no-break space weighs a space here, so a
/// value ending in one compares equal to the value without it: `=` and
/// `COUNT(DISTINCT)` fold the two. `GROUP BY` trims trailing spaces by
/// character instead and keeps every other weight, so it reports them as
/// two groups. The space terminator goes back on, so among the values this
/// key keeps apart it still orders the way the comparison does.
#[must_use]
pub fn unicode_ci_group_key(text: &str) -> Vec<u8> {
    let mut key = UnicodeCiWeights::new(text.trim_end_matches(' ')).collect::<Vec<_>>();
    key.push(UNICODE_CI_SPACE);
    let mut bytes = Vec::with_capacity(key.len() * 2);
    for weight in key {
        bytes.extend_from_slice(&weight.to_be_bytes());
    }
    bytes
}

fn koi8r_weights(text: &str, binary: bool) -> impl Iterator<Item = u16> {
    const WEIGHTS: [u8; 128] = [
        0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e,
        0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d,
        0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xe5, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab,
        0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xe5, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9,
        0xba, 0xbb, 0xbc, 0xbd, 0xfe, 0xdf, 0xe0, 0xf6, 0xe3, 0xe4, 0xf4, 0xe2, 0xf5, 0xe8, 0xe9,
        0xea, 0xeb, 0xec, 0xed, 0xee, 0xef, 0xff, 0xf0, 0xf1, 0xf2, 0xf3, 0xe6, 0xe1, 0xfc, 0xfb,
        0xe7, 0xf8, 0xfd, 0xf9, 0xf7, 0xfa, 0xfe, 0xdf, 0xe0, 0xf6, 0xe3, 0xe4, 0xf4, 0xe2, 0xf5,
        0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef, 0xff, 0xf0, 0xf1, 0xf2, 0xf3, 0xe6, 0xe1,
        0xfc, 0xfb, 0xe7, 0xf8, 0xfd, 0xf9, 0xf7, 0xfa,
    ];
    pintail_types::CharacterSet::Koi8R
        .encode(text)
        .into_iter()
        .map(move |byte| {
            u16::from(if binary {
                byte
            } else if byte < 128 {
                byte.to_ascii_uppercase()
            } else {
                WEIGHTS[usize::from(byte - 128)]
            })
        })
}

/// Compares Cyrillic single-byte values with insignificant trailing spaces.
#[must_use]
pub fn compare_koi8r(left: &str, right: &str, binary: bool) -> std::cmp::Ordering {
    compare_padded(
        koi8r_weights(left, binary),
        koi8r_weights(right, binary),
        u16::from(b' '),
    )
}

/// A Cyrillic byte collation key with the same padding rules as comparison.
#[must_use]
pub fn koi8r_sort_key(text: &str, binary: bool) -> Vec<u8> {
    padded_sort_key(koi8r_weights(text, binary), u16::from(b' '))
}

#[cfg(test)]
mod tests {
    use super::{
        Collation, compare_general_ci, compare_unicode_ci, general_ci_sort_key, general_ci_weight,
        unicode_ci_sort_key,
    };
    use std::cmp::Ordering;

    #[test]
    fn ascii_case_folds() {
        assert_eq!(compare_general_ci("student", "STUDENT"), Ordering::Equal);
        assert_eq!(general_ci_sort_key("a"), general_ci_sort_key("A"));
    }

    #[test]
    fn latin1_accents_fold_to_their_base_letter() {
        // general_ci maps À..Å onto A, which is why it is "ci" but not "ai":
        // it folds these by table rather than by decomposition.
        assert_eq!(general_ci_weight('Ä'), u16::from(b'A'));
        assert_eq!(compare_general_ci("Ärger", "arger"), Ordering::Equal);
    }

    #[test]
    fn every_supplementary_character_is_equal_to_every_other() {
        // Verified against MySQL: 😀 and 𠀀 both weigh 0xFFFD and compare
        // equal. A real MySQL wart, reproduced deliberately.
        assert_eq!(general_ci_weight('😀'), 0xfffd);
        assert_eq!(general_ci_weight('𠀀'), 0xfffd);
        assert_eq!(compare_general_ci("😀", "𠀀"), Ordering::Equal);
    }

    #[test]
    fn trailing_spaces_are_insignificant() {
        // general_ci is PAD SPACE. Found by differential test against MySQL,
        // which reports '' = ' ' as true; the first implementation here said
        // false.
        assert_eq!(compare_general_ci("", " "), Ordering::Equal);
        assert_eq!(compare_general_ci("student", "student   "), Ordering::Equal);
        assert_eq!(general_ci_sort_key("a"), general_ci_sort_key("a  "));
        // Leading and interior spaces still count.
        assert_ne!(compare_general_ci(" a", "a"), Ordering::Equal);
        assert_ne!(compare_general_ci("a b", "ab"), Ordering::Equal);
    }

    #[test]
    fn ordering_is_by_weight_then_length() {
        assert_eq!(compare_general_ci("abc", "abd"), Ordering::Less);
        assert_eq!(compare_general_ci("ab", "abc"), Ordering::Less);
        assert_eq!(compare_general_ci("abc", "ab"), Ordering::Greater);
    }

    #[test]
    fn sort_keys_order_the_same_way_the_comparator_does() {
        // Every hash-based operator uses the key and every ordered one uses
        // the comparator; if they disagreed, a GROUP BY and an ORDER BY over
        // the same column would partition it differently.
        let mut words = ["banana", "Apple", "cherry", "APPLE", "bandana"];
        words.sort_by(|left, right| compare_general_ci(left, right));
        for pair in words.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            assert!(
                general_ci_sort_key(left) <= general_ci_sort_key(right),
                "{left:?} vs {right:?}",
            );
        }
    }

    #[test]
    fn unsupported_names_resolve_to_nothing() {
        assert_eq!(
            Collation::from_mysql_name("utf8mb4_general_ci"),
            Some(Collation::Utf8mb4GeneralCi),
        );
        assert_eq!(
            Collation::from_mysql_name("UTF8MB4_0900_AI_CI"),
            Some(Collation::Utf8mb40900AiCi),
        );
        assert_eq!(
            Collation::from_mysql_name("utf8mb4_unicode_ci"),
            Some(Collation::Utf8mb4UnicodeCi),
        );
        assert_eq!(Collation::from_mysql_name("utf8mb4_croatian_ci"), None);
    }

    #[test]
    fn a_pad_pushes_a_space_against_the_character_it_faces() {
        // Measured on MySQL 8.4, both collations: STRCMP('a', 'a\t') is 1.
        // PAD SPACE pads the shorter operand, which puts a space against the
        // tab, and a space outweighs a tab. Trimming trailing spaces instead
        // makes 'a' a prefix of 'a\t' and answers -1 - the shape this had
        // before, and wrong on every string ending below the space weight.
        assert_eq!(compare_general_ci("a", "a\t"), Ordering::Greater);
        assert_eq!(compare_unicode_ci("a", "a\t"), Ordering::Greater);
        assert_eq!(compare_general_ci("a ", "a\t"), Ordering::Greater);
        assert_eq!(compare_unicode_ci("a ", "a\t"), Ordering::Greater);
        // A character ABOVE the space weight still sorts after the pad.
        assert_eq!(compare_unicode_ci("a", "a!"), Ordering::Less);
    }

    #[test]
    fn unicode_ci_folds_case_and_accents() {
        assert_eq!(compare_unicode_ci("student", "STUDENT"), Ordering::Equal);
        // The combining mark weighs nothing, so the composed and decomposed
        // forms agree and both equal the bare letter.
        assert_eq!(compare_unicode_ci("a\u{301}", "\u{e1}"), Ordering::Equal);
        assert_eq!(compare_unicode_ci("a", "\u{e1}"), Ordering::Equal);
        assert_eq!(unicode_ci_sort_key("Ärger"), unicode_ci_sort_key("arger"));
    }

    #[test]
    fn unicode_ci_expands_the_characters_mysql_expands() {
        // ß weighs as two s weights, so it equals "ss" - the expansion
        // general_ci has no way to express.
        assert_eq!(compare_unicode_ci("\u{df}", "ss"), Ordering::Equal);
        assert_eq!(
            unicode_ci_sort_key("stra\u{df}e"),
            unicode_ci_sort_key("strasse")
        );
        // Verified against MySQL: æ is its own primary weight, NOT "ae".
        assert_ne!(compare_unicode_ci("\u{e6}", "ae"), Ordering::Equal);
    }

    #[test]
    fn unicode_ci_pads_and_ignores_what_weighs_a_space() {
        assert_eq!(compare_unicode_ci("", " "), Ordering::Equal);
        assert_eq!(compare_unicode_ci("student", "student   "), Ordering::Equal);
        assert_eq!(unicode_ci_sort_key("a"), unicode_ci_sort_key("a  "));
        // A no-break space weighs a space, so a trailing one is as
        // insignificant as a space. MySQL's `=` and COUNT(DISTINCT) agree;
        // its GROUP BY does not, which is a MySQL inconsistency recorded in
        // docs/limitations.md rather than one reproduced here.
        assert_eq!(compare_unicode_ci("a", "a\u{a0}"), Ordering::Equal);
        // Leading and interior spaces still count.
        assert_ne!(compare_unicode_ci(" a", "a"), Ordering::Equal);
    }

    #[test]
    fn unicode_ci_collapses_every_supplementary_character() {
        // The same MySQL wart general_ci has, verified for this collation
        // too: WEIGHT_STRING is 0xFFFD for both, and they compare equal.
        assert_eq!(
            compare_unicode_ci("\u{1f600}", "\u{20000}"),
            Ordering::Equal
        );
    }

    #[test]
    fn unicode_ci_orders_a_cjk_ideograph_by_its_code_point() {
        // BMP CJK weighs by UCA's derived rule rather than a table entry.
        assert_eq!(compare_unicode_ci("\u{4e00}", "\u{4e01}"), Ordering::Less);
        // And after Latin, which weighs far below the implicit bases.
        assert_eq!(compare_unicode_ci("z", "\u{4e00}"), Ordering::Less);
    }

    #[test]
    fn unicode_ci_sort_keys_order_the_same_way_the_comparator_does() {
        // Every hash-based operator uses the key and every ordered one uses
        // the comparator; if they disagreed, a GROUP BY and an ORDER BY over
        // the same column would partition it differently. The terminator in
        // the key is what keeps these two in step across a pad.
        let mut words = [
            "banana",
            "Apple",
            "cherry",
            "APPLE",
            "bandana",
            "a",
            "a\t",
            "a ",
            "stra\u{df}e",
            "strasse",
            "\u{4e00}",
        ];
        words.sort_by(|left, right| compare_unicode_ci(left, right));
        for pair in words.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            assert!(
                unicode_ci_sort_key(left) <= unicode_ci_sort_key(right),
                "{left:?} vs {right:?}",
            );
        }
    }
}

#[cfg(test)]
mod latin1_tests {
    use super::Collation;
    use std::cmp::Ordering;

    #[test]
    fn latin1_weights_preserve_padding_and_swedish_letters() {
        let collation = Collation::from_mysql_name("latin1_swedish_ci").expect("Latin-1 collation");
        for (left, right, expected) in [
            ("a\0", "a", Ordering::Less),
            ("a", "A ", Ordering::Equal),
            ("a", "å", Ordering::Less),
            ("å", "ä", Ordering::Less),
            ("ä", "ö", Ordering::Less),
            ("é", "e", Ordering::Equal),
            ("ü", "y", Ordering::Equal),
        ] {
            assert_eq!(
                crate::compare_collated_text(left, right, collation),
                expected,
                "{left:?} versus {right:?}"
            );
        }
    }
}

#[cfg(test)]
mod koi8r_tests {
    use super::{compare_koi8r, koi8r_sort_key};
    use std::cmp::Ordering;
    #[test]
    fn weights_agree_with_keys_and_cyrillic_case() {
        for (left, right, expected) in [
            ("Р", "р ", Ordering::Equal),
            ("Е", "Ё", Ordering::Less),
            ("Я", "Ю", Ordering::Greater),
            ("а\0", "а", Ordering::Less),
        ] {
            assert_eq!(compare_koi8r(left, right, false), expected);
            assert_eq!(
                koi8r_sort_key(left, false).cmp(&koi8r_sort_key(right, false)),
                expected
            );
        }
        assert_eq!(compare_koi8r("а", "А", true), Ordering::Less);
    }
}

const LATIN2_WEIGHTS: [u8; 256] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
    0x40, 0x41, 0x44, 0x45, 0x48, 0x49, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x53, 0x54, 0x56,
    0x58, 0x59, 0x5a, 0x5b, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x68, 0x69, 0x6a, 0x6b, 0x6c,
    0x6d, 0x41, 0x44, 0x45, 0x48, 0x49, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x53, 0x54, 0x56,
    0x58, 0x59, 0x5a, 0x5b, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x6e, 0x6f, 0x70, 0x71, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0x42, 0xff, 0x52, 0xff, 0x51, 0x5c, 0xff, 0xff, 0x5d, 0x5b, 0x5e, 0x65, 0xff, 0x67, 0x66,
    0xff, 0x42, 0xff, 0x52, 0xff, 0x51, 0x5c, 0xff, 0xff, 0x5d, 0x5b, 0x5e, 0x65, 0xff, 0x67, 0x66,
    0x5a, 0x43, 0x43, 0x43, 0x43, 0x51, 0x46, 0x45, 0x47, 0x49, 0x4a, 0x49, 0x49, 0x4e, 0x4e, 0x48,
    0xff, 0x55, 0x54, 0x57, 0x56, 0x56, 0x56, 0xff, 0x5a, 0x5f, 0x5f, 0x5f, 0x5f, 0x63, 0x5e, 0xff,
    0x5a, 0x43, 0x43, 0x43, 0x43, 0x51, 0x46, 0x45, 0x47, 0x49, 0x4a, 0x49, 0x49, 0x4e, 0x4e, 0x48,
    0xff, 0x55, 0x54, 0x57, 0x56, 0x56, 0x56, 0xff, 0x5a, 0x5f, 0x5f, 0x5f, 0x5f, 0x63, 0x5e, 0xff,
];

fn latin2_weights(text: &str, binary: bool) -> impl Iterator<Item = u16> {
    pintail_types::CharacterSet::Latin2
        .encode(text)
        .into_iter()
        .map(move |byte| {
            u16::from(if binary {
                byte
            } else {
                LATIN2_WEIGHTS[usize::from(byte)]
            })
        })
}

/// Central European flat weights, sharing PAD SPACE comparison and key rules.
#[must_use]
pub fn latin2_sort_key(text: &str, binary: bool) -> Vec<u8> {
    padded_sort_key(latin2_weights(text, binary), u16::from(b' '))
}

/// Compare Central European encoded weights, padding the shorter string with spaces.
#[must_use]
pub fn compare_latin2(left: &str, right: &str, binary: bool) -> std::cmp::Ordering {
    compare_padded(
        latin2_weights(left, binary),
        latin2_weights(right, binary),
        u16::from(b' '),
    )
}

fn tis620_weights(text: &str, binary: bool) -> impl Iterator<Item = u16> {
    let bytes = pintail_types::CharacterSet::Tis620.encode(text);
    if binary {
        bytes
    } else {
        thai::positional_weights(&bytes)
    }
    .into_iter()
    .map(u16::from)
}

/// Thai byte weights with insignificant trailing spaces.
#[must_use]
pub fn tis620_sort_key(text: &str, binary: bool) -> Vec<u8> {
    padded_sort_key(tis620_weights(text, binary), u16::from(b' '))
}

/// Compare Thai byte weights, including embedded bytes below the space weight.
#[must_use]
pub fn compare_tis620(left: &str, right: &str, binary: bool) -> std::cmp::Ordering {
    compare_padded(
        tis620_weights(left, binary),
        tis620_weights(right, binary),
        u16::from(b' '),
    )
}
