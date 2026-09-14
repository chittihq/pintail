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
    /// Central European single-byte text.
    Latin2,
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
            "latin2" => Some(Self::Latin2),
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
            Self::Latin2 => "latin2_general_ci",
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
            Self::Utf8Mb4 | Self::Utf8Mb3 | Self::Latin1 | Self::Latin2 | Self::Koi8R => 1,
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => 2,
            Self::Utf32 => 4,
        }
    }

    /// Apply a fixed-width case map when an encoding defines one.
    #[must_use]
    pub fn single_byte_case(self, bytes: &[u8], upper: bool) -> Option<Vec<u8>> {
        if self != Self::Latin2 {
            return None;
        }
        let mapping = if upper { &LATIN2_UPPER } else { &LATIN2_LOWER };
        Some(
            bytes
                .iter()
                .map(|byte| mapping[usize::from(*byte)])
                .collect(),
        )
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
                Self::Latin2 => bytes.push(if character.is_ascii() {
                    character as u8
                } else {
                    LATIN2_EXTENDED
                        .iter()
                        .zip(128..=255)
                        .find_map(|(value, byte)| (*value == character).then_some(byte))
                        .unwrap_or(b'?')
                }),
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
            Self::Latin2 => bytes.iter().map(|byte| latin2_character(*byte)).collect(),
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
            Self::Latin2 => Some(bytes.iter().map(|byte| latin2_character(*byte)).collect()),
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

fn latin2_character(byte: u8) -> char {
    if byte < 128 {
        char::from(byte)
    } else {
        LATIN2_EXTENDED[usize::from(byte - 128)]
    }
}

const LATIN2_EXTENDED: [char; 128] = [
    '\u{80}', '\u{81}', '\u{82}', '\u{83}', '\u{84}', '\u{85}', '\u{86}', '\u{87}', '\u{88}',
    '\u{89}', '\u{8a}', '\u{8b}', '\u{8c}', '\u{8d}', '\u{8e}', '\u{8f}', '\u{90}', '\u{91}',
    '\u{92}', '\u{93}', '\u{94}', '\u{95}', '\u{96}', '\u{97}', '\u{98}', '\u{99}', '\u{9a}',
    '\u{9b}', '\u{9c}', '\u{9d}', '\u{9e}', '\u{9f}', '\u{a0}', '\u{104}', '\u{2d8}', '\u{141}',
    '\u{a4}', '\u{13d}', '\u{15a}', '\u{a7}', '\u{a8}', '\u{160}', '\u{15e}', '\u{164}', '\u{179}',
    '\u{ad}', '\u{17d}', '\u{17b}', '\u{b0}', '\u{105}', '\u{2db}', '\u{142}', '\u{b4}', '\u{13e}',
    '\u{15b}', '\u{2c7}', '\u{b8}', '\u{161}', '\u{15f}', '\u{165}', '\u{17a}', '\u{2dd}',
    '\u{17e}', '\u{17c}', '\u{154}', '\u{c1}', '\u{c2}', '\u{102}', '\u{c4}', '\u{139}', '\u{106}',
    '\u{c7}', '\u{10c}', '\u{c9}', '\u{118}', '\u{cb}', '\u{11a}', '\u{cd}', '\u{ce}', '\u{10e}',
    '\u{110}', '\u{143}', '\u{147}', '\u{d3}', '\u{d4}', '\u{150}', '\u{d6}', '\u{d7}', '\u{158}',
    '\u{16e}', '\u{da}', '\u{170}', '\u{dc}', '\u{dd}', '\u{162}', '\u{df}', '\u{155}', '\u{e1}',
    '\u{e2}', '\u{103}', '\u{e4}', '\u{13a}', '\u{107}', '\u{e7}', '\u{10d}', '\u{e9}', '\u{119}',
    '\u{eb}', '\u{11b}', '\u{ed}', '\u{ee}', '\u{10f}', '\u{111}', '\u{144}', '\u{148}', '\u{f3}',
    '\u{f4}', '\u{151}', '\u{f6}', '\u{f7}', '\u{159}', '\u{16f}', '\u{fa}', '\u{171}', '\u{fc}',
    '\u{fd}', '\u{163}', '\u{2d9}',
];

#[cfg(test)]
mod latin2_tests {
    use super::CharacterSet;

    #[test]
    fn central_european_bytes_round_trip_and_replace_unrepresentable_text() {
        let bytes: Vec<u8> = (0..=255).collect();
        let text = CharacterSet::Latin2.decode(&bytes).unwrap();
        assert_eq!(CharacterSet::Latin2.encode(&text), bytes);
        assert_eq!(
            CharacterSet::Latin2.encode("ĄąČčŁłŐő"),
            [0xa1, 0xb1, 0xc8, 0xe8, 0xa3, 0xb3, 0xd5, 0xf5]
        );
        assert_eq!(
            CharacterSet::Latin2.encode("é😀�€"),
            [0xe9, b'?', b'?', b'?']
        );
    }
}

const LATIN2_UPPER: [u8; 256] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
    0x60, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f,
    0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f,
    0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f,
    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
    0xb0, 0xa1, 0xb2, 0xa3, 0xb4, 0xa5, 0xa6, 0xb7, 0xb8, 0xa9, 0xaa, 0xab, 0xac, 0xbd, 0xae, 0xaf,
    0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf,
    0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf,
    0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf,
    0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xf7, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xff,
];

const LATIN2_LOWER: [u8; 256] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
    0x40, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
    0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f,
    0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f,
    0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f,
    0xa0, 0xb1, 0xa2, 0xb3, 0xa4, 0xb5, 0xb6, 0xa7, 0xa8, 0xb9, 0xba, 0xbb, 0xbc, 0xad, 0xbe, 0xbf,
    0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
    0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef,
    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xd7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xdf,
    0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef,
    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff,
];
