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
