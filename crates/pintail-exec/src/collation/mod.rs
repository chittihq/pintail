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

use general_ci_table::GENERAL_CI_EXCEPTIONS;

/// A text collation the executor can compare.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub enum Collation {
    /// `MySQL` 8's default: UCA 9.0.0, accent- and case-insensitive.
    #[default]
    Utf8mb40900AiCi,
    /// `MySQL` 5.x's default: a flat per-character weight, no expansions or
    /// contractions.
    Utf8mb4GeneralCi,
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
        match name.to_ascii_lowercase().as_str() {
            "utf8mb4_0900_ai_ci" => Some(Self::Utf8mb40900AiCi),
            "utf8mb4_general_ci" => Some(Self::Utf8mb4GeneralCi),
            "utf8mb4_bin" => Some(Self::Utf8mb4Bin),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    #[must_use]
    pub const fn mysql_name(self) -> &'static str {
        match self {
            Self::Utf8mb40900AiCi => "utf8mb4_0900_ai_ci",
            Self::Utf8mb4GeneralCi => "utf8mb4_general_ci",
            Self::Utf8mb4Bin => "utf8mb4_bin",
            Self::Json => "json",
        }
    }
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

/// Trailing spaces, which `general_ci` does not count.
///
/// This collation is PAD SPACE: comparison pads the shorter operand with
/// spaces, which makes trailing spaces insignificant, so `''` equals `' '` and
/// `'a'` equals `'a   '`. `utf8mb4_0900_ai_ci` is NO PAD and does none of
/// this - the two collations genuinely disagree here, which is one more reason
/// the choice cannot be a global constant.
fn without_pad(text: &str) -> &str {
    text.trim_end_matches(' ')
}

/// Compares two strings under `general_ci`.
///
/// Weight by weight, then by length - there are no expansions or contractions
/// in this collation, so one character always yields exactly one weight and
/// the comparison needs no buffering.
#[must_use]
pub fn compare_general_ci(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_chars = without_pad(left).chars();
    let mut right_chars = without_pad(right).chars();
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(left_char), Some(right_char)) => {
                let ordering = general_ci_weight(left_char).cmp(&general_ci_weight(right_char));
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
        }
    }
}

/// Compares two strings under `utf8mb4_bin`: code points by value, trailing
/// spaces insignificant (PAD SPACE, like `general_ci` and unlike `0900_ai_ci`).
#[must_use]
pub fn compare_bin(left: &str, right: &str) -> std::cmp::Ordering {
    without_pad(left).cmp(without_pad(right))
}

/// The `utf8mb4_bin` sort key: the bytes themselves, trailing spaces removed.
#[must_use]
pub fn bin_sort_key(text: &str) -> Vec<u8> {
    without_pad(text).as_bytes().to_vec()
}

/// The `general_ci` sort key, for hashing, grouping, `DISTINCT` and set
/// membership.
///
/// Bytes rather than the hex text the ICU path produces: a key is compared and
/// hashed, never read, so encoding it as text doubles its size for nothing.
#[must_use]
pub fn general_ci_sort_key(text: &str) -> Vec<u8> {
    let text = without_pad(text);
    let mut key = Vec::with_capacity(text.len() * 2);
    for character in text.chars() {
        key.extend_from_slice(&general_ci_weight(character).to_be_bytes());
    }
    key
}

#[cfg(test)]
mod tests {
    use super::{Collation, compare_general_ci, general_ci_sort_key, general_ci_weight};
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
        // unicode_ci is UCA 4.0.0 and genuinely differs; substituting either
        // supported collation for it would produce wrong answers quietly.
        assert_eq!(Collation::from_mysql_name("utf8mb4_unicode_ci"), None);
    }
}
