//! Character encodings at SQL's text/byte boundary. Execution stores Unicode;
//! a character set determines the bytes observed by hashes and byte functions.

/// Encodings with an explicit, loss-aware Unicode conversion.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CharacterSet {
    /// Unicode encoded as UTF-8.
    #[default]
    Utf8Mb4,
    /// Western European single-byte text, including the Windows extensions.
    Latin1,
    /// UTF-8 restricted to the basic multilingual plane.
    Utf8Mb3,
    /// Big-endian basic-plane code units.
    Ucs2,
    /// Big-endian UTF-16, including surrogate pairs.
    Utf16,
    /// Little-endian UTF-16.
    Utf16Le,
    /// Big-endian Unicode scalar values.
    Utf32,
}

/// The longest complete UTF-8 prefix before malformed or incomplete bytes.
#[must_use]
pub fn utf8_prefix(bytes: &[u8]) -> &[u8] {
    let end = std::str::from_utf8(bytes).map_or_else(|error| error.valid_up_to(), str::len);
    &bytes[..end]
}

impl CharacterSet {
    /// Resolve a SQL character-set name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "utf8mb4" => Some(Self::Utf8Mb4),
            "latin1" => Some(Self::Latin1),
            "utf8" | "utf8mb3" => Some(Self::Utf8Mb3),
            "ucs2" => Some(Self::Ucs2),
            "utf16" => Some(Self::Utf16),
            "utf16le" => Some(Self::Utf16Le),
            "utf32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// Default comparison name selected by a connection character set.
    #[must_use]
    pub const fn default_collation(self) -> &'static str {
        match self {
            Self::Utf8Mb4 => "utf8mb4_0900_ai_ci",
            Self::Latin1 => "latin1_swedish_ci",
            Self::Utf8Mb3 => "utf8mb3_general_ci",
            Self::Ucs2 => "ucs2_general_ci",
            Self::Utf16 => "utf16_general_ci",
            Self::Utf16Le => "utf16le_general_ci",
            Self::Utf32 => "utf32_general_ci",
        }
    }

    /// Minimum encoded character width, including introducer padding.
    #[must_use]
    pub const fn minimum_width(self) -> usize {
        match self {
            Self::Utf8Mb4 | Self::Utf8Mb3 | Self::Latin1 => 1,
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => 2,
            Self::Utf32 => 4,
        }
    }

    /// Encode Unicode, replacing characters outside the target repertoire.
    #[must_use]
    pub fn encode(self, text: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(text.len());
        for mut character in text.chars() {
            if matches!(self, Self::Ucs2 | Self::Utf8Mb3) && u32::from(character) > 0xffff {
                character = '?';
            }
            match self {
                Self::Latin1 => bytes.push(latin1_byte(character).unwrap_or(b'?')),
                Self::Utf8Mb4 | Self::Utf8Mb3 => {
                    bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                }
                Self::Ucs2 | Self::Utf16 | Self::Utf16Le => {
                    for unit in character.encode_utf16(&mut [0; 2]) {
                        bytes.extend_from_slice(&if self == Self::Utf16Le {
                            unit.to_le_bytes()
                        } else {
                            unit.to_be_bytes()
                        });
                    }
                }
                Self::Utf32 => bytes.extend_from_slice(&u32::from(character).to_be_bytes()),
            }
        }
        bytes
    }

    /// Align a binary-to-text conversion and validate its encoded units.
    /// UCS-2 admits every code unit at the byte boundary, including surrogates.
    #[must_use]
    pub fn converted_bytes(self, bytes: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
        let width = self.minimum_width();
        let padding = (width - bytes.len() % width) % width;
        let bytes = if padding == 0 {
            std::borrow::Cow::Borrowed(bytes)
        } else {
            let mut padded = vec![0; padding];
            padded.extend_from_slice(bytes);
            std::borrow::Cow::Owned(padded)
        };
        if self != Self::Ucs2 && self.decode(&bytes).is_none() {
            return None;
        }
        Some(bytes)
    }

    /// Decode the complete leading characters of a raw result buffer.
    /// Byte consumers can retain the original buffer independently.
    #[must_use]
    pub fn decode_prefix(self, bytes: &[u8]) -> String {
        match self {
            Self::Latin1 => bytes.iter().map(|byte| latin1_character(*byte)).collect(),
            Self::Utf8Mb4 | Self::Utf8Mb3 => std::str::from_utf8(utf8_prefix(bytes))
                .unwrap_or_default()
                .chars()
                .take_while(|character| self != Self::Utf8Mb3 || u32::from(*character) <= 0xffff)
                .collect(),
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => {
                let units = bytes.chunks_exact(2).map(|pair| {
                    if self == Self::Utf16Le {
                        u16::from_le_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_be_bytes([pair[0], pair[1]])
                    }
                });
                if self == Self::Ucs2 {
                    units
                        .map_while(|unit| char::from_u32(u32::from(unit)))
                        .collect()
                } else {
                    char::decode_utf16(units).map_while(Result::ok).collect()
                }
            }
            Self::Utf32 => bytes
                .chunks_exact(4)
                .map_while(|part| {
                    char::from_u32(u32::from_be_bytes([part[0], part[1], part[2], part[3]]))
                })
                .collect(),
        }
    }

    /// Decode valid encoded text. Ill-formed input has no Unicode carrier.
    #[must_use]
    pub fn decode(self, bytes: &[u8]) -> Option<String> {
        match self {
            Self::Latin1 => Some(bytes.iter().map(|byte| latin1_character(*byte)).collect()),
            Self::Utf8Mb4 | Self::Utf8Mb3 => {
                let text = std::str::from_utf8(bytes).ok()?;
                if self == Self::Utf8Mb3 && text.chars().any(|c| u32::from(c) > 0xffff) {
                    return None;
                }
                Some(text.to_owned())
            }
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => {
                if !bytes.len().is_multiple_of(2) {
                    return None;
                }
                let units = bytes.chunks_exact(2).map(|pair| {
                    if self == Self::Utf16Le {
                        u16::from_le_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_be_bytes([pair[0], pair[1]])
                    }
                });
                if self == Self::Ucs2 {
                    units.map(|unit| char::from_u32(u32::from(unit))).collect()
                } else {
                    char::decode_utf16(units)
                        .collect::<Result<String, _>>()
                        .ok()
                }
            }
            Self::Utf32 => {
                if !bytes.len().is_multiple_of(4) {
                    return None;
                }
                bytes
                    .chunks_exact(4)
                    .map(|part| {
                        char::from_u32(u32::from_be_bytes([part[0], part[1], part[2], part[3]]))
                    })
                    .collect()
            }
        }
    }
}

const LATIN1_EXTENDED: [char; 32] = [
    '€', '\u{0081}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{008d}', 'Ž',
    '\u{008f}', '\u{0090}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{009d}',
    'ž', 'Ÿ',
];

fn latin1_character(byte: u8) -> char {
    match byte {
        0x80..=0x9f => LATIN1_EXTENDED[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

fn latin1_byte(character: char) -> Option<u8> {
    match u32::from(character) {
        value @ (0..=0x7f | 0xa0..=0xff) => u8::try_from(value).ok(),
        _ => LATIN1_EXTENDED
            .iter()
            .position(|candidate| *candidate == character)
            .and_then(|position| u8::try_from(position).ok())
            .map(|position| position + 0x80),
    }
}

#[cfg(test)]
mod latin1_tests {
    use super::CharacterSet;

    #[test]
    fn latin1_preserves_every_byte_and_encodes_extended_characters() {
        let charset = CharacterSet::from_name("latin1").expect("latin1 encoding");
        let bytes = (0_u8..=255).collect::<Vec<_>>();
        let decoded = charset.decode(&bytes).expect("every byte is defined");
        assert_eq!(charset.encode(&decoded), bytes);
        assert_eq!(charset.encode("é€Ÿ🐬"), vec![0xe9, 0x80, 0x9f, b'?']);
        assert_eq!(charset.default_collation(), "latin1_swedish_ci");
    }
}
