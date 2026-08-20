//! `MySQL`'s JSON comparison ladder as an order-preserving byte key.
//!
//! `MySQL` compares two JSON values by type precedence first - of the types
//! JSON text can hold: `BOOLEAN > ARRAY > OBJECT > STRING > NUMBER > NULL` -
//! and within one type by that type's own rule: booleans as `false < true`,
//! arrays element by element with the shorter prefix smaller, strings by
//! bytes, numbers numerically across integer/double. Two objects are equal
//! when they hold the same members; the relative order of unequal objects is
//! documented as unspecified-but-deterministic, so this encoding compares
//! member count first, then members in key order - deterministic, and equal
//! exactly when `MySQL` says equal.
//!
//! Everything here reduces to one primitive: an order-preserving byte key.
//! Comparison, grouping, DISTINCT and set membership all agree because they
//! all compare or hash the same bytes.

use std::cmp::Ordering;

// Ladder tags, low to high. Every tag is >= 0x01 so a sequence terminator
// of 0x00 sorts a shorter array before any longer one sharing its prefix.
const TAG_NULL: u8 = 0x01;
const TAG_NUMBER: u8 = 0x02;
const TAG_STRING: u8 = 0x03;
const TAG_OBJECT: u8 = 0x04;
const TAG_ARRAY: u8 = 0x05;
const TAG_BOOLEAN: u8 = 0x06;

/// The order-preserving key of one JSON document, or `None` when the text
/// is not JSON (callers fall back to a plain text comparison).
#[must_use]
pub(crate) fn json_sort_key(text: &str) -> Option<Vec<u8>> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let mut key = Vec::with_capacity(text.len() + 8);
    encode(&value, &mut key);
    Some(key)
}

/// Ladder comparison of two JSON texts; `None` when either side fails to
/// parse.
#[must_use]
pub(crate) fn compare_json_text(left: &str, right: &str) -> Option<Ordering> {
    Some(json_sort_key(left)?.cmp(&json_sort_key(right)?))
}

fn encode(value: &serde_json::Value, key: &mut Vec<u8>) {
    match value {
        serde_json::Value::Null => key.push(TAG_NULL),
        serde_json::Value::Bool(flag) => {
            key.push(TAG_BOOLEAN);
            key.push(u8::from(*flag));
        }
        serde_json::Value::Number(number) => {
            key.push(TAG_NUMBER);
            encode_f64(number.as_f64().unwrap_or(0.0), key);
        }
        serde_json::Value::String(text) => {
            key.push(TAG_STRING);
            encode_text(text, key);
        }
        serde_json::Value::Array(items) => {
            key.push(TAG_ARRAY);
            for item in items {
                encode(item, key);
            }
            key.push(0x00);
        }
        serde_json::Value::Object(members) => {
            key.push(TAG_OBJECT);
            // Count first, then members sorted by key: '1 = 1' for equal
            // objects regardless of insertion order, and a deterministic
            // order for unequal ones (MySQL leaves it unspecified).
            let mut sorted: Vec<(&String, &serde_json::Value)> = members.iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            key.extend_from_slice(
                &u32::try_from(sorted.len())
                    .unwrap_or(u32::MAX)
                    .to_be_bytes(),
            );
            for (member_key, member_value) in sorted {
                encode_text(member_key, key);
                encode(member_value, key);
            }
        }
    }
}

/// IEEE-754 bits rearranged so unsigned byte order equals numeric order:
/// negative values invert entirely, non-negative values set the sign bit.
fn encode_f64(number: f64, key: &mut Vec<u8>) {
    // JSON has no NaN; -0.0 must equal 0.0.
    let number = if number == 0.0 { 0.0 } else { number };
    let bits = if number.is_sign_negative() {
        !number.to_bits()
    } else {
        number.to_bits() | 0x8000_0000_0000_0000
    };
    key.extend_from_slice(&bits.to_be_bytes());
}

/// Zero-escaped text with a two-byte terminator, so "a" < "ab" and no
/// embedded byte collides with a structural terminator.
fn encode_text(text: &str, key: &mut Vec<u8>) {
    for byte in text.as_bytes() {
        if *byte == 0x00 {
            key.extend_from_slice(&[0x00, 0x01]);
        } else {
            key.push(*byte);
        }
    }
    key.extend_from_slice(&[0x00, 0x00]);
}

#[cfg(test)]
mod tests {
    use super::compare_json_text;
    use std::cmp::Ordering;

    fn cmp(left: &str, right: &str) -> Ordering {
        compare_json_text(left, right).expect("valid JSON")
    }

    #[test]
    fn the_type_ladder_holds() {
        // NULL < NUMBER < STRING < OBJECT < ARRAY < BOOLEAN, low to high.
        let ladder = ["null", "999", r#""zzz""#, r#"{"a":1}"#, "[1]", "false"];
        for pair in ladder.windows(2) {
            assert_eq!(cmp(pair[0], pair[1]), Ordering::Less, "{pair:?}");
        }
    }

    #[test]
    fn numbers_compare_numerically_across_forms() {
        assert_eq!(cmp("1", "1.0"), Ordering::Equal);
        assert_eq!(cmp("-1", "0.5"), Ordering::Less);
        assert_eq!(cmp("10", "9"), Ordering::Greater);
        assert_eq!(cmp("-0.0", "0"), Ordering::Equal);
    }

    #[test]
    fn strings_compare_by_bytes() {
        assert_eq!(cmp(r#""a""#, r#""ab""#), Ordering::Less);
        assert_eq!(cmp(r#""A""#, r#""a""#), Ordering::Less);
        assert_eq!(cmp(r#""b""#, r#""b""#), Ordering::Equal);
    }

    #[test]
    fn arrays_compare_element_wise_with_prefix_rule() {
        assert_eq!(cmp("[1,2]", "[1,3]"), Ordering::Less);
        assert_eq!(cmp("[1,2]", "[1,2,0]"), Ordering::Less);
        assert_eq!(cmp("[2]", "[1,9,9]"), Ordering::Greater);
    }

    #[test]
    fn objects_are_equal_regardless_of_member_order() {
        assert_eq!(cmp(r#"{"a":1,"b":2}"#, r#"{"b":2,"a":1}"#), Ordering::Equal);
        assert_ne!(cmp(r#"{"a":1,"b":2}"#, r#"{"a":1,"b":3}"#), Ordering::Equal);
    }

    #[test]
    fn booleans_order_false_before_true() {
        assert_eq!(cmp("false", "true"), Ordering::Less);
    }
}
