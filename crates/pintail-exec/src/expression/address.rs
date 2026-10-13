//! Network addresses as `MySQL` reads and prints them.
//!
//! `INET6_ATON` turns the text of an IPv4 or IPv6 address into its four or
//! sixteen bytes and `INET6_NTOA` prints those bytes back; `IS_IPV4` and
//! `IS_IPV6` say whether text would be read. The reading is `MySQL`'s own
//! and is stricter than most parsers in some places and looser in others:
//! an IPv4 field may carry leading zeros up to three digits, an IPv6 group
//! at most four hex digits, `::` may stand for a single group, and nothing
//! may surround the address.

use std::fmt::Write as _;

/// The four bytes dotted-quad text spells: four decimal fields of one to
/// three digits, each at most 255, and nothing else.
pub(super) fn ipv4_bytes(text: &[u8]) -> Option<[u8; 4]> {
    if !(7..=15).contains(&text.len()) {
        return None;
    }
    let mut bytes = [0_u8; 4];
    let mut field = 0;
    let mut digits = 0;
    let mut value = 0_u16;
    for byte in text {
        match byte {
            b'0'..=b'9' => {
                digits += 1;
                value = value * 10 + u16::from(byte - b'0');
                if digits > 3 || value > 255 {
                    return None;
                }
            }
            b'.' if digits > 0 && field < 3 => {
                bytes[field] = u8::try_from(value).ok()?;
                field += 1;
                digits = 0;
                value = 0;
            }
            _ => return None,
        }
    }
    if digits == 0 || field != 3 {
        return None;
    }
    bytes[3] = u8::try_from(value).ok()?;
    Some(bytes)
}

/// The sixteen bytes IPv6 text spells: groups of one to four hex digits
/// between colons, one `::` standing for the groups left out, and
/// optionally a dotted quad as the last four bytes.
pub(super) fn ipv6_bytes(text: &[u8]) -> Option<[u8; 16]> {
    if !(2..=39).contains(&text.len()) {
        return None;
    }
    // A leading colon can only be the first half of `::`.
    let text = match text {
        [b':', b':', ..] => &text[1..],
        [b':', ..] => return None,
        _ => text,
    };
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    let mut gap = None;
    let mut group_start = 0;
    let mut digits = 0;
    let mut value = 0_u16;
    for (index, byte) in text.iter().enumerate() {
        match byte {
            b':' => {
                group_start = index + 1;
                if digits == 0 {
                    if gap.is_some() {
                        return None;
                    }
                    gap = Some(filled);
                    continue;
                }
                // A colon that ends a group has to start another.
                if index + 1 == text.len() || filled + 2 > bytes.len() {
                    return None;
                }
                bytes[filled..filled + 2].copy_from_slice(&value.to_be_bytes());
                filled += 2;
                digits = 0;
                value = 0;
            }
            b'.' => {
                if filled + 4 > bytes.len() {
                    return None;
                }
                let quad = ipv4_bytes(&text[group_start..])?;
                bytes[filled..filled + 4].copy_from_slice(&quad);
                filled += 4;
                digits = 0;
                break;
            }
            _ => {
                let digit = u16::try_from(char::from(*byte).to_digit(16)?).ok()?;
                if digits >= 4 {
                    return None;
                }
                value = (value << 4) | digit;
                digits += 1;
            }
        }
    }
    if digits > 0 {
        if filled + 2 > bytes.len() {
            return None;
        }
        bytes[filled..filled + 2].copy_from_slice(&value.to_be_bytes());
        filled += 2;
    }
    if let Some(gap) = gap {
        // `::` stands for at least one group.
        if filled == bytes.len() {
            return None;
        }
        let moved = filled - gap;
        bytes.copy_within(gap..filled, 16 - moved);
        bytes[gap..16 - moved].fill(0);
        filled = bytes.len();
    }
    (filled == bytes.len()).then_some(bytes)
}

/// Either address family's bytes: four for a dotted quad, else sixteen.
pub(super) fn address_bytes(text: &[u8]) -> Option<Vec<u8>> {
    ipv4_bytes(text)
        .map(|bytes| bytes.to_vec())
        .or_else(|| ipv6_bytes(text).map(|bytes| bytes.to_vec()))
}

fn push_quad(text: &mut String, bytes: &[u8]) {
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            text.push('.');
        }
        let _ = write!(text, "{byte}");
    }
}

/// Four bytes as a dotted quad, sixteen as IPv6 text; any other length is
/// not an address.
///
/// The first longest run of zero groups is written `::`, a run of one
/// included. An address whose first six groups are zero, or whose first
/// five are and the sixth is `ffff`, ends in a dotted quad.
pub(super) fn address_text(bytes: &[u8]) -> Option<String> {
    let mut text = String::with_capacity(39);
    if bytes.len() == 4 {
        push_quad(&mut text, bytes);
        return Some(text);
    }
    let bytes: &[u8; 16] = bytes.try_into().ok()?;
    let mut groups = [0_u16; 8];
    for (group, pair) in groups.iter_mut().zip(bytes.chunks_exact(2)) {
        *group = u16::from_be_bytes([pair[0], pair[1]]);
    }
    let (mut gap_start, mut gap_length) = (groups.len(), 0);
    let mut run = None;
    for (index, group) in groups.iter().enumerate() {
        if *group == 0 {
            let (_, length) = run.get_or_insert((index, 0));
            *length += 1;
        } else if let Some((start, length)) = run.take()
            && length > gap_length
        {
            (gap_start, gap_length) = (start, length);
        }
    }
    if let Some((start, length)) = run
        && length > gap_length
    {
        (gap_start, gap_length) = (start, length);
    }
    let mut index = 0;
    while index < groups.len() {
        if index == gap_start {
            if index == 0 {
                text.push(':');
            }
            text.push(':');
            index += gap_length;
            continue;
        }
        if index == 6
            && gap_start == 0
            && (gap_length == 6 || gap_length == 5 && groups[5] == 0xffff)
        {
            push_quad(&mut text, &bytes[12..]);
            break;
        }
        let _ = write!(text, "{:x}", groups[index]);
        if index != 7 {
            text.push(':');
        }
        index += 1;
    }
    Some(text)
}

/// Whether sixteen bytes are an IPv4-compatible address: twelve zero bytes
/// and then an IPv4 address other than `0.0.0.0` and `0.0.0.1`.
pub(super) fn ipv4_compatible(bytes: &[u8]) -> bool {
    bytes.len() == 16
        && bytes[..12].iter().all(|byte| *byte == 0)
        && (bytes[12..15].iter().any(|byte| *byte != 0) || bytes[15] > 1)
}

/// Whether sixteen bytes are an IPv4-mapped address: ten zero bytes, two
/// `ff` bytes, then the IPv4 address.
pub(super) fn ipv4_mapped(bytes: &[u8]) -> bool {
    bytes.len() == 16
        && bytes[..10].iter().all(|byte| *byte == 0)
        && bytes[10] == 0xff
        && bytes[11] == 0xff
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn hex(bytes: Option<Vec<u8>>) -> Option<String> {
        bytes.map(|bytes| {
            bytes.iter().fold(String::new(), |mut text, byte| {
                let _ = write!(text, "{byte:02X}");
                text
            })
        })
    }

    #[test]
    fn text_reads_as_the_bytes_mysql_reads() {
        for (text, expected) in [
            ("10.0.5.9", Some("0A000509")),
            ("01.2.3.4", Some("01020304")),
            ("001.2.3.04", Some("01020304")),
            ("0001.2.3.4", None),
            ("1.2.3", None),
            ("1.2.3.256", None),
            ("1.2.3.4.5", None),
            ("1.2.3.4 ", None),
            ("127", None),
            ("::1", Some("00000000000000000000000000000001")),
            ("::", Some("00000000000000000000000000000000")),
            ("1::", Some("00010000000000000000000000000000")),
            ("1::8", Some("00010000000000000000000000000008")),
            ("FE80::1", Some("FE800000000000000000000000000001")),
            ("1:2:3:4:5:6:7:8", Some("00010002000300040005000600070008")),
            ("1:2:3:4:5:6:7::", Some("00010002000300040005000600070000")),
            ("::2:3:4:5:6:7:8", Some("00000002000300040005000600070008")),
            (
                "1:2:3:4:5:6:1.2.3.4",
                Some("00010002000300040005000601020304"),
            ),
            ("::ffff:10.0.5.9", Some("00000000000000000000FFFF0A000509")),
            ("1::1.2.3.4", Some("00010000000000000000000001020304")),
            ("1:2:3:4:5:6:7", None),
            ("1:2:3:4:5:6:7:8:9", None),
            ("1:2:3:4:5:6:7:1.2.3.4", None),
            ("1::2::3", None),
            (":::", None),
            (":", None),
            ("::00001", None),
            ("12345::", None),
            ("g::1", None),
            (" ::1", None),
            ("::1 ", None),
            ("::1.2.3", None),
            ("::1.2.3.256", None),
            ("fe80::1%eth0", None),
            ("", None),
        ] {
            assert_eq!(
                hex(address_bytes(text.as_bytes())).as_deref(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn bytes_print_as_mysql_prints_them() {
        for (text, expected) in [
            ("10.0.5.9", "10.0.5.9"),
            ("fdfe::5a55:caff:fefa:9089", "fdfe::5a55:caff:fefa:9089"),
            ("::", "::"),
            ("::1", "::1"),
            ("::0.1.0.0", "::0.1.0.0"),
            ("::10.0.5.9", "::10.0.5.9"),
            ("::ffff:10.0.5.9", "::ffff:10.0.5.9"),
            ("::ffff:0:1", "::ffff:0.0.0.1"),
            ("0:0:0:0:0:ffff:0:0", "::ffff:0.0.0.0"),
            ("::fffe:1.2.3.4", "::fffe:102:304"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("1:0:0:0:2:0:0:3", "1::2:0:0:3"),
            ("0:0:1:0:0:0:0:0", "0:0:1::"),
            ("1:2:3:4:5:6:7:0", "1:2:3:4:5:6:7::"),
            ("0:2:3:4:5:6:7:8", "::2:3:4:5:6:7:8"),
            ("1:0:3:4:5:6:7:8", "1::3:4:5:6:7:8"),
            ("ABCD:EF01::", "abcd:ef01::"),
        ] {
            let bytes = address_bytes(text.as_bytes()).expect(text);
            assert_eq!(address_text(&bytes).as_deref(), Some(expected), "{text}");
        }
        assert_eq!(address_text(b"12345"), None);
        assert_eq!(address_text(b""), None);
    }

    #[test]
    fn embedded_ipv4_forms_are_told_apart() {
        let bytes = |text: &str| address_bytes(text.as_bytes()).expect(text);
        assert!(ipv4_compatible(&bytes("::10.0.5.9")));
        assert!(!ipv4_compatible(&bytes("::ffff:10.0.5.9")));
        assert!(!ipv4_compatible(&bytes("::1")));
        assert!(!ipv4_compatible(&bytes("::")));
        assert!(!ipv4_compatible(&bytes("1.2.3.4")));
        assert!(ipv4_mapped(&bytes("::ffff:10.0.5.9")));
        assert!(!ipv4_mapped(&bytes("::10.0.5.9")));
        assert!(!ipv4_mapped(&bytes("1.2.3.4")));
    }
}
