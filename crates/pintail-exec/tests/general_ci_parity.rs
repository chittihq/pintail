//! Differential check of  against `MySQL` itself.
//!
//! The corpus, `MySQL`'s ordering of it, and every pair `MySQL` calls equal were
//! captured from a live `MySQL` 8 server. A weight table can look plausible and
//! still be wrong; this is what makes it checkable.

use pintail_exec::collation::{compare_general_ci, general_ci_sort_key};

const CORPUS: &[&str] = &[
    "student",
    "STUDENT",
    "Student",
    "teacher",
    "admin",
    "ADMIN",
    "superadmin",
    "lat",
    "partner",
    "support",
    "active",
    "archived",
    "draft",
    "published",
    "hold",
    "refunded",
    "completed",
    "invited",
    "discontinued",
    "Ärger",
    "arger",
    "ÅNGSTROM",
    "angstrom",
    "café",
    "CAFE",
    "naïve",
    "naive",
    "ß",
    "ss",
    "SS",
    "é",
    "e",
    "E",
    "ñ",
    "n",
    "ö",
    "o",
    "ü",
    "u",
    "Ω",
    "ω",
    "Σ",
    "σ",
    "ς",
    "日本",
    "中文",
    "😀",
    "𠀀",
    "",
    " ",
    "_",
    "-",
    "0",
    "9",
];

/// `MySQL`'s ordering, tie-broken by binary value so the expectation is total
/// rather than dependent on sort stability.
const MYSQL_ORDER: &[&str] = &[
    "",
    " ",
    "-",
    "0",
    "9",
    "active",
    "ADMIN",
    "admin",
    "angstrom",
    "ÅNGSTROM",
    "archived",
    "arger",
    "Ärger",
    "CAFE",
    "café",
    "completed",
    "discontinued",
    "draft",
    "E",
    "e",
    "é",
    "hold",
    "invited",
    "lat",
    "n",
    "ñ",
    "naive",
    "naïve",
    "o",
    "ö",
    "partner",
    "published",
    "refunded",
    "ß",
    "SS",
    "ss",
    "STUDENT",
    "Student",
    "student",
    "superadmin",
    "support",
    "teacher",
    "u",
    "ü",
    "_",
    "Σ",
    "ς",
    "σ",
    "Ω",
    "ω",
    "中文",
    "日本",
    "😀",
    "𠀀",
];

/// Index pairs `MySQL` reports as equal under `general_ci`.
const MYSQL_EQUAL: &[(usize, usize)] = &[
    (0, 1),
    (0, 2),
    (1, 2),
    (4, 5),
    (19, 20),
    (21, 22),
    (23, 24),
    (25, 26),
    (28, 29),
    (30, 31),
    (30, 32),
    (31, 32),
    (33, 34),
    (35, 36),
    (37, 38),
    (39, 40),
    (41, 42),
    (41, 43),
    (42, 43),
    (46, 47),
    (48, 49),
];

#[test]
fn ordering_matches_mysql() {
    let mut ours: Vec<&str> = CORPUS.to_vec();
    ours.sort_by(|left, right| compare_general_ci(left, right).then_with(|| left.cmp(right)));
    assert_eq!(ours, MYSQL_ORDER, "ordering diverges from MySQL");
}

#[test]
fn equality_matches_mysql() {
    for (left, left_text) in CORPUS.iter().enumerate() {
        for (right, right_text) in CORPUS.iter().enumerate().skip(left + 1) {
            let expected = MYSQL_EQUAL.contains(&(left, right));
            let actual = compare_general_ci(left_text, right_text).is_eq();
            assert_eq!(
                actual, expected,
                "{left_text:?} vs {right_text:?}: MySQL says equal={expected}",
            );
        }
    }
}

#[test]
fn sort_keys_agree_with_equality() {
    for (index, left_text) in CORPUS.iter().enumerate() {
        for right_text in CORPUS.iter().skip(index + 1) {
            assert_eq!(
                general_ci_sort_key(left_text) == general_ci_sort_key(right_text),
                compare_general_ci(left_text, right_text).is_eq(),
                "{left_text:?} vs {right_text:?}",
            );
        }
    }
}

/// The executor's dispatch, not just the collation module.
///
/// `compare_collated_text` is what every operator funnels through, so this
/// checks the switch itself: the same pair of strings must answer differently
/// under the two collations. Trailing spaces are the sharpest case -
/// `general_ci` is PAD SPACE and `0900_ai_ci` is not - so a dispatch that fell
/// back silently
/// to the default would fail here rather than pass by coincidence.
#[test]
fn the_executor_dispatches_on_the_collation_it_was_given() {
    use pintail_exec::collation::Collation;
    use std::cmp::Ordering;

    for (left, right) in [("student", "student   "), ("", " "), ("a", "a  ")] {
        assert_eq!(
            pintail_exec::compare_collated_text(left, right, Collation::Utf8mb4GeneralCi),
            Ordering::Equal,
            "general_ci pads: {left:?} vs {right:?}",
        );
        assert_ne!(
            pintail_exec::compare_collated_text(left, right, Collation::Utf8mb40900AiCi),
            Ordering::Equal,
            "0900_ai_ci does not pad: {left:?} vs {right:?}",
        );
    }

    // Case and accent folding agree between the two, so they cannot be used to
    // tell the dispatch apart - asserted so a future change that breaks one
    // does not look like a dispatch failure.
    for (left, right) in [("student", "STUDENT"), ("Ärger", "arger")] {
        for collation in [Collation::Utf8mb4GeneralCi, Collation::Utf8mb40900AiCi] {
            assert_eq!(
                pintail_exec::compare_collated_text(left, right, collation),
                Ordering::Equal,
                "{left:?} vs {right:?} under {collation:?}",
            );
        }
    }
}
