//! The `utf8mb4_0900_ai_ci` sort key of printable ASCII text, from a table.
//!
//! The collator walks its data for every character of every value: about a
//! microsecond for a 36-character identifier, paid per row by every join,
//! group and window keyed on such a column. For printable ASCII the key is
//! simpler than that walk: no character contracts with another or expands
//! past its own weight, so the bytes a character adds depend only on the
//! character before it (a run of weights sharing a lead byte writes that
//! byte once, and what closes a run depends on where the next one starts).
//!
//! Nothing here knows those rules. The table is the collator's own answer:
//! built once from the keys it writes for every single character and every
//! pair, and refused whole - the collator then answers everything - if any
//! pair's key does not begin with its first character's key. The bytes
//! appended are therefore the collator's bytes, which the tests check for
//! every string of up to three characters and for long random ones.

use std::sync::OnceLock;

/// The bytes one character adds to a key, after a given class of character.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Added {
    len: u8,
    bytes: [u8; Self::MAX],
}

impl Added {
    const MAX: usize = 7;
    /// The character is not answered from the table.
    const UNKNOWN: Self = Self {
        len: u8::MAX,
        bytes: [0; Self::MAX],
    };

    fn of(bytes: &[u8]) -> Option<Self> {
        let mut added = Self {
            len: u8::try_from(bytes.len()).ok()?,
            bytes: [0; Self::MAX],
        };
        added.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(added)
    }
}

type Row = [Added; 128];

pub(crate) struct AsciiKeys {
    /// What each character adds: row 0 at the start of a value, row
    /// `class_of[previous]` after another character.
    rows: Vec<Row>,
    class_of: [u8; 128],
}

impl AsciiKeys {
    /// The table read off `collator_key`, which appends the collator's key
    /// of a text; `None` when printable ASCII does not behave as the module
    /// says.
    fn build(collator_key: &dyn Fn(&str, &mut Vec<u8>)) -> Option<Self> {
        let key = |text: &[u8]| {
            let mut out = Vec::new();
            collator_key(std::str::from_utf8(text).ok()?, &mut out);
            Some(out)
        };
        if !key(b"")?.is_empty() {
            return None;
        }
        let mut singles: Vec<Option<Vec<u8>>> = vec![None; 128];
        let mut first = [Added::UNKNOWN; 128];
        for character in b' '..=b'~' {
            let single = key(&[character])?;
            // A character that adds nothing leaves the next one's bytes to
            // the character before it: left to the collator.
            if single.is_empty() {
                continue;
            }
            first[usize::from(character)] = Added::of(&single)?;
            singles[usize::from(character)] = Some(single);
        }
        let mut rows = vec![first];
        let mut class_of = [0_u8; 128];
        for previous in b' '..=b'~' {
            let Some(single) = &singles[usize::from(previous)] else {
                continue;
            };
            let mut row = [Added::UNKNOWN; 128];
            for character in b' '..=b'~' {
                if singles[usize::from(character)].is_none() {
                    continue;
                }
                let pair = key(&[previous, character])?;
                row[usize::from(character)] = Added::of(pair.strip_prefix(single.as_slice())?)?;
            }
            let class = if let Some(known) = rows.iter().skip(1).position(|known| *known == row) {
                known + 1
            } else {
                rows.push(row);
                rows.len() - 1
            };
            class_of[usize::from(previous)] = u8::try_from(class).ok()?;
        }
        Some(Self { rows, class_of })
    }

    /// Appends the key of `text` and answers `true`, or leaves `out` as it
    /// was and answers `false` for a text the table does not cover.
    fn append(&self, text: &str, out: &mut Vec<u8>) -> bool {
        let start = out.len();
        let mut row = &self.rows[0];
        for byte in text.bytes() {
            let Some(added) = row
                .get(usize::from(byte))
                .filter(|added| added.len != u8::MAX)
            else {
                out.truncate(start);
                return false;
            };
            out.extend_from_slice(&added.bytes[..usize::from(added.len)]);
            row = &self.rows[usize::from(self.class_of[usize::from(byte)])];
        }
        true
    }
}

static KEYS: OnceLock<Option<AsciiKeys>> = OnceLock::new();

/// Appends the `utf8mb4_0900_ai_ci` key of `text`: from the table when the
/// text is printable ASCII, from `collator_key` otherwise. The bytes are
/// the collator's either way.
pub(crate) fn append_key(text: &str, out: &mut Vec<u8>, collator_key: &dyn Fn(&str, &mut Vec<u8>)) {
    let covered = KEYS
        .get_or_init(|| AsciiKeys::build(collator_key))
        .as_ref()
        .is_some_and(|keys| keys.append(text, out));
    if !covered {
        collator_key(text, out);
    }
}

#[cfg(test)]
mod tests {
    use super::{AsciiKeys, KEYS, append_key};
    use crate::execution::collator_ai_ci_key;

    fn collator(text: &str) -> Vec<u8> {
        let mut key = Vec::new();
        collator_ai_ci_key(text, &mut key);
        key
    }

    fn tabled(text: &str) -> Vec<u8> {
        // A prefix already in the buffer must survive, covered or not.
        let mut key = vec![0xAB, 0xCD];
        append_key(text, &mut key, &collator_ai_ci_key);
        assert_eq!(&key[..2], &[0xAB, 0xCD]);
        key.split_off(2)
    }

    fn keys() -> &'static AsciiKeys {
        KEYS.get_or_init(|| AsciiKeys::build(&collator_ai_ci_key))
            .as_ref()
            .expect("the collator's keys for printable ASCII fit the table")
    }

    /// Every printable ASCII character is answered from the table (or this
    /// test would compare the collator with itself), and every string of up
    /// to three of them has the collator's key, byte for byte.
    #[test]
    fn every_ascii_string_of_up_to_three_characters_has_the_collators_key() {
        let keys = keys();
        let printable = (b' '..=b'~').collect::<Vec<_>>();
        for character in &printable {
            let mut out = Vec::new();
            assert!(
                keys.append(std::str::from_utf8(&[*character]).unwrap(), &mut out),
                "character {character:#x} is not in the table"
            );
        }
        assert_eq!(tabled(""), collator(""));
        let mut text = String::new();
        for first in &printable {
            text.clear();
            text.push(char::from(*first));
            assert_eq!(tabled(&text), collator(&text), "{text:?}");
            for second in &printable {
                text.truncate(1);
                text.push(char::from(*second));
                assert_eq!(tabled(&text), collator(&text), "{text:?}");
                for third in &printable {
                    text.truncate(2);
                    text.push(char::from(*third));
                    assert_eq!(tabled(&text), collator(&text), "{text:?}");
                }
            }
        }
    }

    /// What a key costs from the collator and from the table, for
    /// identifiers of 36 characters. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_key_cost() {
        let ids = (0..200_000_u64)
            .map(|id| {
                let hash = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    hash >> 32,
                    hash & 0xffff,
                    (hash >> 16) & 0xffff,
                    id & 0xffff,
                    hash & 0xffff_ffff_ffff
                )
            })
            .collect::<Vec<_>>();
        let _ = keys();
        let mut out = Vec::with_capacity(128);
        for round in 0..5 {
            let started = std::time::Instant::now();
            let mut bytes = 0;
            for id in &ids {
                out.clear();
                collator_ai_ci_key(id, &mut out);
                bytes += out.len();
            }
            let from_collator = started.elapsed();
            let started = std::time::Instant::now();
            let mut tabled_bytes = 0;
            for id in &ids {
                out.clear();
                append_key(id, &mut out, &collator_ai_ci_key);
                tabled_bytes += out.len();
            }
            let from_table = started.elapsed();
            assert_eq!(bytes, tabled_bytes);
            println!(
                "round {round}: collator {:.0} ns/key, table {:.0} ns/key",
                from_collator.as_secs_f64() * 1e9 / 200_000.0,
                from_table.as_secs_f64() * 1e9 / 200_000.0
            );
        }
    }

    /// Long random strings: printable ASCII alone (the table), and with
    /// control characters, accents, expansions and wide characters mixed in
    /// (the collator), in one buffer after another.
    #[test]
    fn random_strings_have_the_collators_key() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let others = [
            "\u{e9}",
            "\u{df}",
            "\u{ff21}",
            "e\u{301}",
            "\t",
            "\u{0}",
            "\u{7f}",
            "\u{4e2d}",
            "\u{1f600}",
            "\u{a0}",
        ];
        let (mut covered, mut uncovered) = (0, 0);
        for round in 0..60_000_u64 {
            let mut text = String::new();
            for _ in 0..next(72) {
                if round % 4 == 3 && next(9) == 0 {
                    let pick = next(u64::try_from(others.len()).unwrap());
                    text.push_str(others[usize::try_from(pick).unwrap()]);
                } else {
                    text.push(char::from(b' ' + u8::try_from(next(95)).unwrap()));
                }
            }
            if keys().append(&text, &mut Vec::new()) {
                covered += 1;
            } else {
                uncovered += 1;
            }
            assert_eq!(tabled(&text), collator(&text), "{text:?}");
        }
        assert!(
            covered > 40_000 && uncovered > 5_000,
            "{covered} {uncovered}"
        );
        // The shapes the fast path exists for.
        for text in [
            "6f1c2a9e-0b7d-4c3a-9e21-5d8a7b6c4f10",
            "6F1C2A9E-0B7D-4C3A-9E21-5D8A7B6C4F10",
            "trailing space ",
            "  leading",
            "a~b!c@d#e$f%g^h&i*j(k)l_m+n-o=p[q]r{s}t|u;v:w'x\"y,z.<>/?`\\",
        ] {
            assert!(keys().append(text, &mut Vec::new()), "{text:?}");
            assert_eq!(tabled(text), collator(text), "{text:?}");
        }
        assert_eq!(
            tabled("6f1c2a9e-0b7d"),
            tabled("6F1C2A9E-0B7D"),
            "case folds in the key"
        );
        assert_ne!(tabled("a"), tabled("a "), "a trailing space is significant");
    }
}
