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
    /// Cyrillic single-byte text.
    Koi8R,
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
            "koi8r" => Some(Self::Koi8R),
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
            Self::Koi8R => "koi8r_general_ci",
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
            Self::Utf8Mb4 | Self::Utf8Mb3 | Self::Latin1 | Self::Koi8R => 1,
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
                Self::Koi8R => bytes.push(if character.is_ascii() {
                    character as u8
                } else {
                    KOI8R_EXTENDED
                        .iter()
                        .zip(128..=255)
                        .find_map(|(value, byte)| (*value == character).then_some(byte))
                        .unwrap_or(b'?')
                }),
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
            Self::Koi8R => bytes.iter().map(|byte| koi8r_character(*byte)).collect(),
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
            Self::Koi8R => Some(bytes.iter().map(|byte| koi8r_character(*byte)).collect()),
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

const KOI8R_EXTENDED: [char; 128] = [
    '\u{2500}', '\u{2502}', '\u{250c}', '\u{2510}', '\u{2514}', '\u{2518}', '\u{251c}', '\u{2524}',
    '\u{252c}', '\u{2534}', '\u{253c}', '\u{2580}', '\u{2584}', '\u{2588}', '\u{258c}', '\u{2590}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2320}', '\u{25a0}', '\u{2219}', '\u{221a}', '\u{2248}',
    '\u{2264}', '\u{2265}', '\u{a0}', '\u{2321}', '\u{b0}', '\u{b2}', '\u{b7}', '\u{f7}',
    '\u{2550}', '\u{2551}', '\u{2552}', '\u{451}', '\u{2553}', '\u{2554}', '\u{2555}', '\u{2556}',
    '\u{2557}', '\u{2558}', '\u{2559}', '\u{255a}', '\u{255b}', '\u{255c}', '\u{255d}', '\u{255e}',
    '\u{255f}', '\u{2560}', '\u{2561}', '\u{401}', '\u{2562}', '\u{2563}', '\u{2564}', '\u{2565}',
    '\u{2566}', '\u{2567}', '\u{2568}', '\u{2569}', '\u{256a}', '\u{256b}', '\u{256c}', '\u{a9}',
    '\u{44e}', '\u{430}', '\u{431}', '\u{446}', '\u{434}', '\u{435}', '\u{444}', '\u{433}',
    '\u{445}', '\u{438}', '\u{439}', '\u{43a}', '\u{43b}', '\u{43c}', '\u{43d}', '\u{43e}',
    '\u{43f}', '\u{44f}', '\u{440}', '\u{441}', '\u{442}', '\u{443}', '\u{436}', '\u{432}',
    '\u{44c}', '\u{44b}', '\u{437}', '\u{448}', '\u{44d}', '\u{449}', '\u{447}', '\u{44a}',
    '\u{42e}', '\u{410}', '\u{411}', '\u{426}', '\u{414}', '\u{415}', '\u{424}', '\u{413}',
    '\u{425}', '\u{418}', '\u{419}', '\u{41a}', '\u{41b}', '\u{41c}', '\u{41d}', '\u{41e}',
    '\u{41f}', '\u{42f}', '\u{420}', '\u{421}', '\u{422}', '\u{423}', '\u{416}', '\u{412}',
    '\u{42c}', '\u{42b}', '\u{417}', '\u{428}', '\u{42d}', '\u{429}', '\u{427}', '\u{42a}',
];

fn koi8r_character(byte: u8) -> char {
    if byte < 128 {
        char::from(byte)
    } else {
        KOI8R_EXTENDED[usize::from(byte - 128)]
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

#[cfg(test)]
mod koi8r_tests {
    use super::CharacterSet;
    #[test]
    fn all_single_byte_values_round_trip() {
        let bytes: Vec<u8> = (0..=255).collect();
        let text = CharacterSet::Koi8R.decode(&bytes).unwrap();
        assert_eq!(CharacterSet::Koi8R.encode(&text), bytes);
        assert_eq!(CharacterSet::Koi8R.encode("РС🐬"), vec![0xf2, 0xf3, b'?']);
    }
}
